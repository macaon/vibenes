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
- `~/Git/puNES/src/core/` — `cpu_inline.h`, `apu.c`.
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

## Current phase handoff (Phase 5 — interrupt polling)

> Written so a fresh agent on another machine can resume without
> backtracking. Last-confirmed state: branch `phase5-interrupt-polling`
> at commit `3fecaef` (as of 2026-04-17). Verify with `git log
> --oneline -3` before acting on it.

**Where we are.** `cpu_interrupts_v2` progression is 2/5 passing:
`1-cli_latency` ✅, `2-nmi_and_brk` ✅, `3-nmi_and_irq` ❌,
`4-irq_and_dma` ❌, `5-branch_delays_irq` ❌. No regressions
anywhere else (run the full sweep in §Tests before committing).

**Architectural change that unlocked test 2.** Bus cycle is now split
into [`tick_pre_access`](src/bus.rs) (PPU tick + NMI edge latch) and
[`tick_post_access`](src/bus.rs) (APU + mapper + IRQ line refresh).
PPU register reads now see mid-cycle PPU state — required for
sync_vbl-style sync loops to line up with real hardware. `tick_cycle`
is kept as a combined entry for DMA stall cycles with no bus access.

**NMI hijack model** (both BRK inline in [src/cpu/ops.rs](src/cpu/ops.rs)
and IRQ service in [src/cpu/mod.rs](src/cpu/mod.rs)): after push phase,
if `bus.prev_nmi_pending` is set, redirect vector to `$FFFA` and
clear `bus.nmi_pending`. Always clear `bus.prev_nmi_pending` at end
of the service so a late NMI (cycles 6–7) is deferred to after the
handler's first instruction — this matches Mesen2's explicit
`_prevNeedNmi = false` at end of `BRK()` ([NesCpu.cpp:238]).

**Remaining Phase 5 sub-items** (plan order — each its own commit):

1. **Sub-B: branch-delays-IRQ** (`5-branch_delays_irq`). On a taken
   branch with no page cross, the IRQ poll on the final cycle is
   suppressed. Snapshot `irq_line_at_start = bus.irq_line` in
   `Cpu::step`, add `branch_taken_no_cross: bool` on `Cpu` set by
   `branch()` in [src/cpu/ops.rs](src/cpu/ops.rs) around line 291,
   then in `poll_interrupts_at_end` skip IRQ latch when the flag is
   set AND IRQ was newly asserted during the branch. Reference:
   puNES `BRC` macro (`cpu.c:114-144`), Mesen2 `NesCpu.h:432-448`.
2. **Debug test 3 `3-nmi_and_irq`**. Hijack mechanism is correct,
   but iterations alternate between pass/fail with anomalous early
   NMI fires on odd iterations. Suspects: (a) APU frame-counter IRQ
   assertion timing relative to mid-cycle PPU tick; (b) something
   in how `cli` delay interacts with the new bus split. Worth
   instrumenting before writing code — print (cycle, iter, nmi
   state, irq state) to isolate which iter starts to diverge.
3. **Sub-C: DMC-DMA ↔ IRQ** (`4-irq_and_dma`). Stall cycles in
   `Bus::service_pending_dmc_dma` use `tick_cycle` — they snapshot
   `prev_*` correctly. Test still fails; need to diagnose exact
   failure (check output against test source). References:
   `reference/apu.md §10 DMC DMA CPU stall`, `reference/punes-notes.md`
   (4-way DMC DMA stall taxonomy).

**Do-before-starting checklist** (every commit on this branch):

```
cargo build --release && cargo test --lib --release
for rom in ~/Git/nes-test-roms/instr_test-v5/official_only.nes \
           ~/Git/nes-test-roms/instr_misc/instr_misc.nes \
           ~/Git/nes-test-roms/apu_test/rom_singles/*.nes \
           ~/Git/nes-test-roms/apu_reset/*.nes \
           ~/Git/nes-test-roms/cpu_interrupts_v2/rom_singles/*.nes; do
  printf "%-30s " "$(basename $rom)"
  ./target/release/test_runner "$rom" 2>&1 | tail -1 | grep -oE 'PASS|FAIL'
done
```

Any regression in the first four groups (instr, apu_test, apu_reset,
first two cpu_interrupts) is a stop-and-investigate signal. Don't
paper over it.

**What worked well this session.** The user pushed back when we hit
a "1-cycle shift" issue and almost punted. Investigating showed it
was bus-access ordering (PPU tick after access vs Mesen2's split
around). Keep that diagnostic habit — when a test is off by exactly
one cycle, question whether sub-systems see the right state at the
right moment, not just whether the CPU logic is right.
