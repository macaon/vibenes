# Phase 11 — DMA iter-alignment follow-up

Written for a fresh-context session to pick up cold. The CPU/PPU/APU
cores are 100% on hardware test suites; this is the last non-mapper
outstanding work.

## 1. Failing ROMs (the whole picture)

Three ROMs fail their ROM-internal CRC-strict checks. All three are
DMC/OAM DMA edge cases. Integration tests in
`tests/dmc_dma_during_read4.rs` gate these on behavior invariants
(pattern shape, replay count) and are **green** — the emulator's
observable behavior is correct, but the ROMs' specific CRCs require
a level of sub-cycle timing accuracy that our current model doesn't
quite reach.

| ROM | Expected | Our output | CRC |
|---|---|---|---|
| `dmc_dma_during_read4/dma_4016_read.nes` | `08 08 07 08 08` | `08 08 08 07 08` | ours=`7C6FDB7E`, golden=`F0AB808C` |
| `dmc_dma_during_read4/dma_2007_read.nes` | `11 22 / 11 22 / 33 44 / 11 22 / 11 22` **or** `.../44 55/...` | `11 22 / 11 22 / 11 22 / 44 55 / 11 22` | lands 1 iter off (in `44 55` bucket) |
| `sprdma_and_dmc_dma.nes` + `_512.nes` | stable per-iter cycle count | varies 525–529 across iters | — |

Pattern: **all three are off by 1 iter** relative to hardware. For
`dma_4016_read` and `dma_2007_read` the hit lands on iter 3 instead
of iter 2. For the sprdma pair, the per-iter cycle count isn't
stable because the DMC fires at different positions within OAM DMA
across iters (hardware stays at a consistent position).

## 2. What's been implemented (don't reinvent)

All on `main` as of commit `3c2620b`:

1. **Full Nestopia DMA cycle-count taxonomy**
   [src/bus.rs](../../src/bus.rs) `service_pending_dmc_dma` and
   `service_pending_dmc_dma_on_write`:
   - `DMC_NORMAL` → 4 cycles (standalone read-cycle DMA).
   - `DMC_CPU_WRITE` → 3 cycles (DMC fires during a CPU write —
     write is absorbed as the halt cycle). Called from `Bus::write`
     via `service_pending_dmc_dma_on_write`.
   - `DMC_R4014` → 2 cycles (DMC fires mid-OAM-DMA — halt + dummy
     absorbed into sprite-DMA cycles).
   - `DMC_NNL_DMA` → 1 cycle (DMC fires on OAM idx 253).
   - `DMC_CPU_WRITE` variant at OAM idx 255 → 3 cycles.

2. **Nestopia Peek+StealCycles structure**
   `peek_with_side_effects(addr)` performs register-state mutations
   (PPU buffer advance, controller shift, $4015 frame-IRQ clear)
   without advancing the master clock. DMC service now does N peeks
   (3 non-$4xxx, 1 $4xxx) up-front, then `StealCycles(N)`, then the
   DMC fetch — mirroring `NstApu.cpp:2282-2333` exactly.

3. **Master-clock-driven PPU**
   [src/clock.rs](../../src/clock.rs) `start_cpu_cycle(is_read)` +
   `end_cpu_cycle(is_read)` replaced the fixed-dot-count model. Each
   CPU cycle's start phase advances master by 5 (read) / 7 (write),
   end phase by the complement. PPU runs to
   `master_cycles - ppu_offset` (`ppu_offset = 1` per Mesen2
   `NesCpu.cpp:154`). `pending_ppu_ticks` hack removed.

4. **Mesen2-matching DMC arm condition**
   [src/apu/dmc.rs](../../src/apu/dmc.rs): DMA only arms from the
   buffer-drain path when buffer was non-empty (`buffer_was_present`
   gate) AND `enable_dma_delay == 0`, so the enable-delay path and
   buffer-drain path can't race each other.

5. **OAM-DMA position tracking**
   [src/bus.rs](../../src/bus.rs) `oam_dma_active` + `oam_dma_idx`
   so `service_pending_dmc_dma` picks the right per-position stall.

## 3. What's been tried (and didn't shift anything)

All of these were empirically verified to produce zero change in
`dma_4016_read`, `dma_2007_read`, `sprdma_and_dmc_dma` output:

- **Stall-count sweeps** (2, 3, 4, 5, 6) — sub-4 hangs `sync_dmc`
  (parity-convergence issue, see below); 4+ all hit iter 3.
- **Enable-delay parity sweeps** (1/2, 2/3, 3/4, 4/5, 1/3, 2/4, …) —
  no shift.
- **`ppu_offset` sweeps** (0, 1, 2, 3) — no shift. Whatever initial
  PPU-CPU phase, `sync_dmc` compensates by syncing before the
  measurement.
- **Service-before-tick vs service-after-tick** (move DMC service
  from BEFORE `tick_pre_access` to AFTER) — no shift.
- **DMC side-effect reads at pending_addr**: replaying halt via
  `self.read(pending_addr)` (multiple reads, each ticking) vs
  Nestopia's model (peeks up-front then pure stall) — no shift on
  `dma_4016_read`; shifted `dma_2007_read` between `33 44` ↔ `44 55`
  buckets (both source-accepted).
- **Switching DMA service's fetch tick to use direct mapper access
  (no bus re-entry)** — equivalent.

The phase-9 follow-up notes (see `notes/phase9/follow_ups.md`) also
dropped a full DMA-refactor branch for unrelated-looking reasons
that in hindsight map to the same issue: `sync_dmc`'s drift-based
exit converges differently depending on parity rules, but the
**observable iter** doesn't shift.

## 4. Key empirical data (don't reinvestigate)

Cycle trace of `dma_4016_read` (instrumented via eprintln at DMA
service entry):

- 39 DMA fires total across the full test run.
- **Every single one** fires at `cpu_cycles & 1 == 0` (even cycle).
- Zero fires on write cycles (`dma-w` trace count = 0). So the
  `DMC_CPU_WRITE` 3-cycle case is live code but never triggered
  by this specific test.
- Gap between consecutive DMA fires = **exactly 3424 CPU cycles**
  (DMC byte period at rate 0), confirming the DMC timer + bit
  counter + buffer-drain loop is clocking correctly.
- First DMA fire at cycle 208616.

sync_dmc outer loop per-iter CPU cost (counted from source, sync_dmc.s):

```
lda #227            ; 2
bne @first          ; 3
inner sbc loop      ; 226 × 15 + 1 × 14 = 3404
lda #$10            ; 2
sta SNDCHN          ; 4
nop                 ; 2
bit SNDCHN          ; 4
bne @wait taken     ; 3
---------------------
total               ; 3424 (exactly DMC period)
+ DMC DMA cycles    ; +4 (DMC_NORMAL)
---------------------
per-iter            ; 3428 cycles
```

Phase-9 note claims iter length is "3421 + DMA". Our count is
3424 + 4 = 3428. The discrepancy is probably a branch-cycle or
alignment-cycle detail (page-cross on bne, `.align 64` etc.) — worth
verifying with a CPU-trace tool before investing more.

## 5. The standing hypothesis

The 1-iter offset on `dma_4016_read` (≈25 CPU cycles of drift) is
**not** in the DMA service code or the PPU tick split. It's in one
of:

a) **DMC timer's event attribution to the "correct" CPU cycle**.
   Puñes' `dmc_tick` (apu.h:181) runs INSIDE `tick_hw` — the DMC
   fetch happens at the CURRENT CPU cycle (inline), not the next
   one like our model. The cycle ADDITION happens via `hwtick[0] +=
   tick` which makes `tick_hw` loop N more iterations — but the
   buffer is non-empty at cycle X, not X+4. This changes when
   `bytes_remaining` decrements relative to CPU operations.

b) **sync_dmc's `.align 64` + branch-cross interactions**. The
   outer loop's `bne @wait` might page-cross on real hardware (4
   cycles) vs not on us (3 cycles), accumulating drift.

c) **A 1-cycle bias in the CPU's "penultimate-cycle" IRQ poll**
   specifically for `bit $4015` — since `bit` is a 4-cycle op (abs)
   and the poll timing on the penultimate cycle might differ
   between our model and hardware when DMC IRQ transitions mid-op.

(a) is the strongest candidate. It would require restructuring
DMC's fetch path to be synchronous with the arm (inline inside the
per-cycle APU tick) rather than deferred to the next bus access.
That's a more invasive refactor than (b) or (c).

## 6. Concrete next-session plan

**Setup:**

1. Build Mesen2 or install a Mesen2 binary (`~/Git/Mesen2` has
   source; requires Mono/.NET 6+ and SDL2). Verify Mesen2 passes
   `dma_4016_read` (if it doesn't, the golden CRC is unit-specific
   and we need to accept a different target).
2. Add a PC+cycle logger to our emulator (gated behind an env var)
   that emits `{cpu_cycles} PC={pc} opcode={op} master={master} ppu={ppu}`
   one line per instruction.
3. Add matching Mesen2 trace via their built-in trace logger.

**Diff workflow:**

1. Run both emulators on `dma_4016_read.nes` for, say, 250k cycles.
2. Align both traces to the same start (first `sei` of sync_dmc, or
   similar known-landmark).
3. `diff` the two traces line-by-line. The first divergence is
   where our model goes wrong.
4. If the diff is an opcode-boundary shift of N cycles, bisect into
   CPU / DMA / PPU to find the source.

**Likely fix:**

Restructure DMC to **fetch inline at buffer-empty** (puNES model,
`apu.h:209-247`) rather than arming `dma_pending` + deferring. This
is a moderate rewrite — touches [src/apu/dmc.rs](../../src/apu/dmc.rs)
and [src/bus.rs](../../src/bus.rs) — but contained to the DMA layer.

Alternative: build Mesen2's lazy `NeedToRun`-driven APU (`NesApu.cpp:180-201`)
on top of our master-clock foundation. Deeper but more robust.

## 7. Definition of done

All four ROMs green via the external runner (`test_runner` +
`blargg_2005_report`):

- `dmc_dma_during_read4/dma_4016_read.nes` — CRC `F0AB808C`.
- `dmc_dma_during_read4/dma_2007_read.nes` — no check_crc, so any
  source-accepted bucket (`33 44` at iter 2 or `44 55` at iter 2).
- `sprdma_and_dmc_dma.nes` — stable cycle count matching blargg's
  golden CRC `B8EA17D9` (our current ours doesn't match, and the
  numbers drift across iters instead of staying stable).
- `sprdma_and_dmc_dma_512.nes` — same golden CRC.

Zero regressions on the full sweep:

```
for rom in ~/Git/nes-test-roms/instr_test-v5/official_only.nes \
           ~/Git/nes-test-roms/instr_misc/instr_misc.nes \
           ~/Git/nes-test-roms/apu_test/rom_singles/*.nes \
           ~/Git/nes-test-roms/apu_reset/*.nes \
           ~/Git/nes-test-roms/cpu_interrupts_v2/rom_singles/*.nes \
           ~/Git/nes-test-roms/ppu_vbl_nmi/rom_singles/*.nes \
           ~/Git/nes-test-roms/oam_read/*.nes \
           ~/Git/nes-test-roms/oam_stress/*.nes \
           ~/Git/nes-test-roms/ppu_open_bus/*.nes \
           ~/Git/nes-test-roms/ppu_read_buffer/*.nes \
           ~/Git/nes-test-roms/cpu_dummy_writes/*.nes \
           ~/Git/nes-test-roms/cpu_exec_space/*.nes \
           ~/Git/nes-test-roms/cpu_reset/*.nes \
           ~/Git/nes-test-roms/nes_instr_test/rom_singles/*.nes \
           ~/Git/nes-test-roms/instr_timing/instr_timing.nes \
           ~/Git/nes-test-roms/instr_test-v3/official_only.nes; do
  r=$(./target/release/test_runner "$rom" 2>&1 | tail -1)
  if ! echo "$r" | grep -qE PASS; then echo "FAIL $(basename $rom)"; fi
done
```

Plus `cargo test --release` all green (115 unit + 11 apu + 5 dmc).

## 8. Cross-references

- [notes/phase9/follow_ups.md](../phase9/follow_ups.md) — prior
  DMA refactor attempts and lessons.
- [notes/phase9/dmc_double_read.md](../phase9/dmc_double_read.md) —
  `dma_4016_read` / `dma_2007_read` detailed analysis.
- `~/Git/Mesen2/Core/NES/APU/DeltaModulationChannel.cpp` — reference
  DMC implementation.
- `~/Git/Mesen2/Core/NES/NesCpu.cpp:325-448` — `ProcessPendingDma`
  get/put-cycle DMA loop.
- `~/Git/punes/src/core/apu.h:181-247` — `dmc_tick` inline-fetch
  model.
- `~/Git/punes/src/core/cpu_inline.h:1374-1398` — OAM DMA with DMC
  tick-type assignments at indices 253/254/255.
- `~/Git/nestopia/source/core/NstApu.cpp:2282-2333` — `DoDMA`
  Peek+StealCycles model.
