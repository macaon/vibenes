# Phase 9 follow-ups

Two DMC/DMA issues that remain after phase 9 lands. Both are
documented here so a future phase can pick them up cold.

---

## F1 — 1-cycle DMC-cycle alignment offset

**Symptom.** In `dmc_dma_during_read4/dma_2007_read.nes` and
`dma_4016_read.nes`, the correct hardware behavior appears but at
iteration `N+1` of the 5-iter sweep instead of iteration `N`. Concrete
outputs:

| ROM | Expected | Our output | Delta |
|---|---|---|---|
| `dma_4016_read` | `08 08 07 08 08` (CRC `F0AB808C`) | `08 08 08 07 08` (CRC `7C6FDB7E`) | `07` lands on row 3 vs row 2 |
| `dma_2007_read` | `11 22 / 11 22 / 33 44` (or `44 55`) `/ 11 22 / 11 22` (CRC `159A7A8F` or `5E3DF9C4`) | `11 22 / 11 22 / 11 22 / 33 44 / 11 22` (CRC `0FED8C78`) | `33 44` lands on row 3 vs row 2 |

The *mechanism* is right — DMC DMA halt cycle replays the CPU's
pending read address (controller bit consumed / PPU buffer advanced);
Mesen2's `skipDummyReads` gating is implemented (halt+dummy replay at
`$2007`, halt-only at `$4016`/`$4017`). The *timing* of WHICH iteration
aligns with the LDA's read cycle is off by exactly 1 CPU cycle.

### What was tried and failed

- **Mesen2-style `_transferStartDelay`** on `$4015` DMC-enable
  (2 or 3 CPU cycles, parity-based). Implemented. sync_dmc absorbed
  the delay and the iter alignment didn't shift.
- **Service order flip** inside `Bus::read` — run APU tick inside
  `tick_pre_access` BEFORE `service_pending_dmc_dma` vs AFTER. No
  effect on the test output.
- **`dmc_dma_complete` reorder** — commit DMC state BEFORE the 4th
  tick_cycle vs AFTER. No effect.
- **DMC timer period tweak** (`rate - 1` vs `rate - 2`). Not actually
  tried because the accumulated drift would break every other
  sample-rate-sensitive test.

### What probably fixes it

An instrumented cycle-by-cycle trace of:
- our APU/DMC state at every CPU cycle during the fine-sync loop of
  `sync_dmc.s` (reading `$4015` for DMC IRQ);
- Mesen2's same trace, running the same ROM;
- the diff point is the cycle at which our `$4015` read first returns
  `DMC IRQ set`. That cycle is what `sync_dmc` latches as its
  anchor; a 1-cycle difference there cascades into every downstream
  iter alignment.

Primary suspects (order of likelihood, no empirical confirmation):
1. When `dmc_dma_complete` sets `dmc_irq` vs when a same-cycle
   `$4015` read observes it (our phase-6 `tick_pre_access` APU-tick
   interacts with this).
2. The exact cycle at which `Dmc::tick_cpu` fires its last-bit shift
   relative to when the DMA is requested — our `tick_cpu` combines
   timer decrement + bit shift + potential DMA-arm in one call.
   Mesen splits these across a more granular state machine with
   `_bufferEmpty` / `_transferStartDelay` / `_needHalt` flags.
3. Interaction between `service_pending_dmc_dma`'s halt-replay read
   and the APU tick inside `tick_pre_access` (nested `Bus::read`
   ticks the APU *again* during the halt cycle; that's an extra
   DMC timer tick that real hardware's halted bus doesn't produce).

### Integration-test gate

`tests/dmc_dma_during_read4.rs` validates the hardware-behavior
invariants (pattern shape, replay count) rather than the ROM's
exact CRC. All 5 tests pass there; the ROM-internal CRC check
still fails on 2. Future fix should flip the integration tests back
to strict CRC matches.

---

## F2 — OAM + DMC DMA interleave (`sprdma_and_dmc_dma.nes`)

**Symptom.** Both `sprdma_and_dmc_dma.nes` and `sprdma_and_dmc_dma_512.nes`
print a table that alternates `528`/`529` cycles per iteration with
CRC `B8EA17D9`. Real hardware produces a stable number (not the
alternation) because DMC DMA gets interleaved with OAM DMA's read
cycles rather than serialized after.

### Root cause (from the phase-9 investigation notes §4)

`Bus::run_oam_dma` is an opaque 513/514-cycle block. It does not call
`service_pending_dmc_dma` inside the 256-iteration read+write loop,
so any DMC DMA request that arms during OAM DMA waits until OAM DMA
finishes and then runs as a stand-alone 4-cycle stall — adding to the
total instead of folding into the existing bus-busy cycles.

Mesen2 (`NesCpu.cpp:399-447`) runs OAM DMA as an explicit get/put
cycle loop; when `_dmcDmaRunning && !_needHalt && !_needDummyRead`
coincides with a get cycle, that cycle becomes the DMC read and the
sprite byte read is postponed by one cycle. Halt/dummy cycles are
absorbed into surrounding sprite-DMA dummy reads via the
`_needHalt`/`_needDummyRead` boolean pair.

### Why we deferred

Phase-9 investigation notes flagged this HIGH regression risk. The
phase-7 `extra_idle = even` parity flip (`src/bus.rs:183`) was tuned
for `cpu_interrupts_v2/4-irq_and_dma` assuming no interleave; any
change to OAM DMA's cycle arithmetic has to preserve that test
green. Unit tests at `src/bus.rs:377-407` (`oam_dma_halt_on_get_*`
/ `oam_dma_halt_on_put_*`) also pin the current no-DMC baseline.

### What a fix looks like

1. Rewrite `run_oam_dma` as an explicit get/put cycle loop (one
   iteration per CPU cycle, sprite-byte read on even cycles, write
   on odd cycles, with a running counter).
2. Factor `service_pending_dmc_dma`'s halt/dummy/align/read phases
   into callable units that `run_oam_dma` can splice in.
3. Re-derive the `extra_idle` parity rule against Mesen2's shape
   (the current even-cycle check was tuned for the opaque model).
4. Re-run the phase-5 checklist (`cpu_interrupts_v2/4-irq_and_dma`
   especially) to confirm no regression.

Estimated scope: 1–2 days of focused work + investigation notes
time. Touches bus/DMA/DMC interfaces — branch before starting.

---

## Cross-reference

Both follow-ups share the same DMC state-machine surface. If F1's
fix requires granular `_needHalt`/`_needDummyRead`-style flags
(hypothesis #2 above), F2's rewrite naturally re-uses them. Good
odds they land together.

---

## Refactor attempt — 2026-04-20 (dropped)

Spent a session on a `dma-refactor` branch trying to port Mesen2's
`ProcessPendingDma`/get-put-loop model. Branch deleted — no commits
worth keeping. Concrete lessons so the next attempt skips them:

1. **Mesen2's parity model alone is insufficient.** Mesen's rule —
   halt-cycle even → 4-cycle DMA, odd → 3-cycle — converges to one
   parity across sync_dmc iters. With iter length 3421 + 3 = 3424
   (DMC period), halt parity is preserved every iter, so drift is
   zero and sync_dmc loops forever. The pre-refactor "always
   4-cycle" model happens to produce iter = 3425 which gives
   drift = 1 cycle/iter — that's why it reaches the "forever:"
   loop at all (just at a 1-iter-off alignment).

2. **The three references disagree on the cycle-count rule.**

   | ref | normal | CPU write | on $4014 | mid-OAM | OAM cyc 254 | OAM cyc 255 |
   |---|---|---|---|---|---|---|
   | Nestopia `NstApu.cpp:2282-2330` | 4 | 3 | — | 2 | 1 | 3 |
   | puNES `apu.h:210-220` | 4 (DMC_NORMAL) | 3 (DMC_CPU_WRITE) | 2 (DMC_R4014) | — | 1 (DMC_NNL_DMA) | — |
   | Mesen2 `NesCpu.cpp:399-447` | 3 or 4 (get/put parity) | — | — | absorbed into sprite DMA cycles | — | — |

   Nestopia and puNES converge on a case-taxonomy; Mesen2 uses a
   uniform get/put loop that the sprite-DMA path splices into.
   For our tests the case-taxonomy is easier to verify
   incrementally (each case has a clear symptom: `dma_4016_read`
   iteration, OAM-overlap cycle count, etc.).

3. **Nestopia's extra-Peek rule is the cleanest $2007 model.**
   `NstApu.cpp:2313-2328`: when DMC DMA collides with a CPU read
   AT THE SAME cycle, the DMA unit does `Peek(readAddress)` two
   extra times (for non-$4xxx addresses) plus one `Peek` for the
   halt — totalling **3 buffer advances** at $2007. The
   `readAddress & 0xF000 != 0x4000` check elides those advances
   for $4016/$4017, where `/OE` is held through the dummy reads
   and the controller only shifts on the halt.

4. **Structural ordering matters more than I expected.** Our
   `Bus::read` runs `tick_pre_access` first (advancing cpu_cycles
   for the CPU's "original read" cycle) and then services DMA
   inline. Mesen runs `ProcessPendingDma` before `StartCpuCycle`,
   so DMA cycles end with cpu_cycles positioned at the halt
   address, and the original read is cycle N+DMA_len. Moving
   service before `tick_pre_access` (as the refactor did) aligned
   the halt cycle with Mesen's model but broke sync_dmc for the
   parity-convergence reason above.

### Recommended next-session setup

- Start from the "nestopia cycle-steal" model, not Mesen's get/put
  loop. Cheaper to get right per-test: the 5 nestopia cases map
  directly to the 5 ROMs in `dmc_dma_during_read4`.
- Plumb the CPU's `IsWriteCycle` / `GetOamDMACycle` equivalents
  into the APU so `DoDMA` can pick the right case. This requires
  a small CPU↔Bus API addition we don't have today.
- Keep the structural `Bus::read` layout unchanged. The fix is
  in `service_pending_dmc_dma`: vary cycle count by case, do the
  right number of `Peek(readAddress)` calls per case.
- Accept that this is a 1–2 day job with several test regressions
  to chase. Don't start it unless there's time to finish it.
