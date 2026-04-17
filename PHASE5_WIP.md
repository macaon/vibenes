# Phase 5 — In-flight state (interrupt polling)

**Branch:** `phase5-interrupt-polling`
**Latest commit:** Sub-B landed — branch-delays-IRQ quirk with unit tests.
**Base commit:** `07ddbee` on `main`.

Verify with `git log --oneline main..HEAD` before resuming — if the
tip has moved, trust the git log over this file.

## Phases roadmap (Phase 5 is current)

| # | Phase | Status |
|---|---|---|
| 1 | APU channels + frame counter | ✅ merged on main |
| 2 | DMC DMA bus stall | ✅ merged on main |
| 3 | $4015 polish + DMC edges | ✅ merged on main |
| 4 | Reset/power-up accuracy + runner reset support | ✅ merged on main |
| 5 | **Penultimate-cycle IRQ/NMI polling** | 🔶 **in progress (this branch)** |
| 6 | OAM DMA 513/514 parity | ✅ merged on main |
| 7 | Full regression sweep + manifest runner | ⏳ pending after 5 |

## Phase 5 sub-item progress

| Sub | Target ROM | Status |
|---|---|---|
| A | `2-nmi_and_brk` | ✅ PASS (commit `3fecaef`) |
| A | `3-nmi_and_irq` | ❌ FAIL — hijack mechanism lands, alternating rows |
| B | `5-branch_delays_irq` | 🔶 logic landed + unit-tested; ROM still FAIL |
| C | `4-irq_and_dma` | ⏳ not started |

`1-cli_latency` passes from the baseline commit `1093d67`.

**Sub-B note.** The branch-delays-IRQ quirk (taken-no-cross suppresses
IRQ latch when IRQ was newly asserted during the branch) is implemented
in [src/cpu/ops.rs](src/cpu/ops.rs) `branch()` + [src/cpu/mod.rs](src/cpu/mod.rs)
`poll_interrupts_at_end`, and verified by two unit tests
(`taken_no_cross_branch_delays_irq_by_one_instruction`,
`branch_not_taken_does_not_delay_irq`). The ROM test `5-branch_delays_irq`
still FAILs, but the failure is in the `test_jmp` subtest which uses
pure JMP — no branches at all. That's the same baseline IRQ timing
issue as test 3 (anomalous IRQ/NMI recognition on specific iteration
phases) and must be fixed before test 5 can pass end-to-end.

## What's in the branch

### `1093d67` — penultimate-cycle polling baseline

- Added `Bus::prev_irq_line` / `Bus::prev_nmi_pending`, snapshotted at
  the start of `tick_cycle`.
- Rewrote `Cpu::step` to poll at end-of-instruction using those
  snapshots (= end-of-penultimate-cycle state).
- CLI/SEI/PLP use `i_flag_before` for the I-flag in the poll; RTI and
  everything else use current I.
- Ships `1-cli_latency`. instr_test-v5 still 16/16.

### `3fecaef` — Sub-A: hijack + bus split + defer

Three related changes, landed together because the hijack's correctness
depends on PPU accesses seeing mid-cycle state.

1. **NMI hijack** in `src/cpu/mod.rs::service_interrupt` (IRQ path)
   and `src/cpu/ops.rs` case `0x00` (BRK inline). After push phase,
   if `bus.prev_nmi_pending` is set → vector becomes `$FFFA` and the
   NMI latch is consumed. Pushed P stays as the caller set it (BRK=1,
   IRQ=0). NMI cannot hijack its own service.
2. **Post-service NMI defer**: always clear `bus.prev_nmi_pending` at
   end of BRK and at end of `service_interrupt` for IRQ/BRK. A late
   NMI (arrived in cycles 6–7, missed hijack) is then deferred to
   after the handler's first instruction — matches Mesen2's explicit
   `_prevNeedNmi = false` at end of `BRK()` (NesCpu.cpp:238).
3. **Bus cycle split**: `tick_pre_access` runs before the CPU bus
   access (advance clock, tick PPU, latch NMI edge via `poll_nmi`);
   `tick_post_access` runs after (tick APU, tick mapper, refresh
   IRQ line from APU). `tick_cycle` kept as combined entry for DMA
   stall cycles that have no bus access. This makes `bit $2002` and
   other PPU register reads see mid-cycle PPU state — the specific
   cycle alignment `sync_vbl` is tuned for.

Also: removed `Cpu::nmi_seen`. Redundant with `bus.nmi_pending`
clearing on service and rising-edge-only PPU detection (matches
Mesen2's simpler `_needNmi` model — no "already serviced" flag).

## What to do next

Proceed through the remaining sub-items per the plan. Each commit:

1. Write/modify code.
2. Run the full regression sweep (see CLAUDE.md §Current phase handoff).
3. Only commit if the target gating ROM is PASS AND no earlier gate regressed.
4. Commit message format: `feat(cpu): phase 5 sub-X — <what>` (no AI
   attribution, no co-author trailer).

### Investigate test 3 `3-nmi_and_irq` (and `5-branch_delays_irq` test_jmp)

**Current output (last seen):**
```
21, 21, 20, 21, 20, 21, 20, 25, 20, 25, 25, 25
```
**Expected:**
```
23, 21, 21, 20, 20, 20, 20, 20, 20, 20, 25, 25
```

- Row 1 expected `23` but got `21` — NMI fires 1 instruction later
  than expected. With the bus split this regressed from the
  pre-split output which had `23` correct in row 1.
- Rows 4–10 all expected `20` (NMI hijacks IRQ consistently). We
  get `20` on rows 4, 6, 10 and anomalous `21`, `25` on odd rows.

The `21` pattern is specifically "NMI fires before `lda #1` executed"
which is VERY early. That's not a hijack-boundary issue — something
is latching NMI at a completely wrong time on those iterations.

**Test 5 `test_jmp` has the same class of bug.** Pure JMP loop, no
branches — our CK column is 0 on every row where expected CK is ≥2,
meaning the IRQ is recognized 1–2 frames earlier than it should be on
specific iteration phases. Same "anomalous early recognition on some
iterations" shape as test 3.

Suspects:
- APU frame-counter IRQ latches on one half of the APU get/put
  phase but the PPU VBlank-set lands on the other half; our new
  mid-cycle PPU visibility might have shifted the relative phase.
- Something subtle with `cli` delay and the new bus split — check
  that `i_flag_before` is captured at the right moment.
- A separate NMI edge firing on `sta PPUCTRL` when VBlank is already
  set (the $2000-write-with-VBlank-set edge case).
- `bus.irq_line` is refreshed in `tick_post_access` only — if an APU
  event in `tick_pre_access` (e.g. PPU-tick-driven state that feeds
  back into APU phase) needs to be visible *before* the CPU's read,
  the current ordering may miss it by one cycle.

Instrument before coding: log `(cycle, pending_interrupt, op, pc)`
around iterations 4–6 of test 3 AND compare the `test_jmp` phase of
test 5 (both should diverge on the same arithmetic boundary).

### Sub-C — DMC DMA ↔ IRQ (`4-irq_and_dma`)

Last — speculative. Budget a diagnostic phase before writing code.
Our stall cycles call `tick_cycle` which now routes through pre+post,
so interrupt polling should already run per stall cycle. The exact
failure mode needs fresh eyes.

## Fallback procedures

**If `instr_test-v5` regresses:** revert the specific commit, run it
isolated, bisect against it. Do NOT paper over the regression.

**If Bash breaks mid-session:** save state to this file AND the
project memory before offering restart. `bash_transient_failure.md`
in memory documents the pattern.

## Environment

- Working directory: `/home/marcus/Git/vibenes2`
- Remote: `ssh://git@git.home.arpa:2222/marcus/vibenes2.git`
- Git user: `macaon <marcus@skogangen.se>`
- Commits: `<type>: <description>` format; no `Co-Authored-By` trailer;
  no AI attribution.
