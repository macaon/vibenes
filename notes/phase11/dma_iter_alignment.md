# Phase 11 — DMA iter-alignment

**STATUS: closed.** The two `dmc_dma_during_read4` ROMs that needed
ROM-internal CRC-strict alignment now pass with the golden CRCs.
`sprdma_and_dmc_dma_512.nes` is the only outstanding ROM in this
class — see [§5 follow-up](#5-follow-up-sprdma_and_dmc_dma_512nes).

## 1. The fix that landed

Two changes on branch `dma-getput-rewrite`, merged after a full
regression sweep with zero failures:

### Parity-aware DMC standalone DMA cost (commit b413b09)

Replace the hardcoded 4-cycle stall in
`Bus::service_pending_dmc_dma` with Mesen2's parity-driven formula
(NesCpu.cpp:325-448):
- **even entry-cycle → 3 cycles** (halt + dummy + DMC-read)
- **odd entry-cycle → 4 cycles** (halt + dummy + align + DMC-read)

Side-effect peek count tracks the stall (1 for `$4016`/`$4017` —
Mesen's `skipDummyReads`; stall-1 for `$2007` and other non-`$4xxx`).
The standalone `service_pending_dmc_dma_on_write` was deleted —
Mesen's `MemoryWrite` (NesCpu.cpp:241-251) deliberately doesn't
service DMA on writes, and the previous "halt absorbed by write"
3-cycle branch was a misread of Nestopia's parity-driven 3-vs-4
split.

### DMC reset-tick alignment (same commit)

Initialize `Dmc::timer` to `period - 1` instead of `period`. Mesen2's
reset path (NesCpu.cpp:160-164) runs 8 dummy CPU cycles where ours
runs 7 (5 dummy reads + 2 vector reads); both end with cpu_cycles=7
but Mesen ticks DMC one extra time. Without this offset our DMC
bit-shifts land one CPU cycle later than Mesen, flipping the
entry-parity at every DMA fire and breaking the sync_dmc convergence
loop in `dmc_dma_during_read4` (the regression noted in
[../phase9/follow_ups.md §"refactor attempt"](../phase9/follow_ups.md)).

### Parity-driven mid-OAM DMC cost (commit 85f06f1)

The DMC stall during OAM DMA is also parity-driven (Mesen's get/put
loop NesCpu.cpp:399-447): the DMC-read steals a get cycle that would
have been an OAM read, deferring it one cycle. Net cost is
`standalone - 1`:
- even entry → 2 cycles
- odd entry → 3 cycles

Replaces the per-idx case taxonomy that no fixed assignment of
253/254/255 → 1/2/3 made consistent with hardware on
`sprdma_and_dmc_dma_512.nes`.

## 2. How the fix was found

Land a per-instruction trace on both emulators that emits the same
field set, then `diff` them:

- **Mesen side** — `tools/trace_mesen.sh <rom> <limit>` runs Mesen2
  in `--testRunner` headless mode against `tools/mesen_trace.lua`
  (templated via sed since Mesen's Lua sandbox strips `os.getenv`).
  Lua API: `addMemoryCallback(emu.callbackType.exec, …)` for the
  per-instruction line, plus `read`/`write` callbacks at $4015/$4016/
  $4017/$2007/$4014/$4010 for DMA-relevant register events.
  `displayMessage` is the only Lua → stdout path that actually fires
  in headless mode (Mesen needs both `--enableStdout` AND
  `--preferences.disableOsd=true`; without the latter the message
  goes to OSD, not stdout).
- **Our side** — `VIBENES_TRACE_LIMIT=N` env-gates a one-line-per-
  instruction print from `cpu/trace.rs`. Zero overhead when unset.

```sh
# Capture both at the same window
VIBENES_TRACE_LIMIT=250000 ./target/release/test_runner ROM > /tmp/v.log
tools/trace_mesen.sh ROM 250000 > /tmp/m.log

# Diff on (cyc, pc, op) only — masks out fields that are expected
# to differ (mclk semantics, dtim from initial state)
diff <(awk '/^\[M\]/ { gsub(/[a-z]+=/, ""); print $2,$3,$4 }' /tmp/v.log) \
     <(awk '/^\[M\]/ { gsub(/[a-z]+=/, ""); print $2,$3,$4 }' /tmp/m.log) | head
```

The first line that diverges is where our DMA model breaks. For
`dma_4016_read.nes` it was line 50967 (cyc=178831 STA $4015), where
ours inserted 4 stall cycles vs Mesen's 3. The fix narrowed the diff
to ~3000 more cycles of agreement before the next case (mid-OAM DMC).

## 3. Definition of done — achieved

| ROM | Result | CRC |
|---|---|---|
| `dmc_dma_during_read4/dma_4016_read.nes` | **PASS** `08 08 07 08 08` | `F0AB808C` ✓ |
| `dmc_dma_during_read4/dma_2007_read.nes` | **PASS** `44 55` at iter 2 | `5E3DF9C4` ✓ (one of two sanctioned) |
| `dmc_dma_during_read4/dma_2007_write.nes` | **PASS** | — |
| `dmc_dma_during_read4/double_2007_read.nes` | **PASS** | `F018C287` (one of four sanctioned) |
| `dmc_dma_during_read4/read_write_2007.nes` | **PASS** | — |
| `sprdma_and_dmc_dma.nes` | **PASS** | hardware-equivalent cycle pattern |

Integration tests in `tests/dmc_dma_during_read4.rs` are now
**strict-pattern** (no more "exactly one 07 anywhere"); they assert
the exact byte sequence.

Zero regressions on the full sweep (apu_test 8/8, apu_reset 6/6,
cpu_interrupts_v2 5/5 incl. `4-irq_and_dma`, ppu_vbl_nmi 10/10,
oam_*, ppu_open_bus, cpu_dummy_writes, instr_test-v5 16/16,
instr_misc 4/4, instr_timing, nes_instr_test 11/11, blargg_apu_2005
11/11, plus 115 unit tests).

## 4. What didn't work and why

- **Per-idx case taxonomy for mid-OAM cost** (`oam_dma_idx == 253 →
  1` etc., from puNES `apu.h:209-247`): no assignment of integers to
  253/254/255 produced Mesen's `sprdma_512` cycle pattern. The
  underlying mechanism is parity, not idx-position.
- **`service_pending_dmc_dma_on_write` (3-cycle "halt absorbed by
  write" branch)**: based on a misread of Nestopia
  `NstApu.cpp:2295`. The 3-vs-4 distinction there is parity-driven,
  not write-vs-read. With it gone, the standalone path on the next
  read does the right thing.
- **Pure parity fix WITHOUT the DMC reset offset**: hangs sync_dmc
  in a forever-loop because all iters land at the same parity. The
  reset offset is what introduces the cross-iter parity variance
  that lets sync_dmc converge.

## 5. Follow-up — `sprdma_and_dmc_dma_512.nes`

Still fails ROM-internal CRC. Cycle pattern is off by 1 cycle on
2 of 16 iters vs Mesen:

```
Mesen (Passed):     525,526,525,526,524,525,526,527,527,528,…
Ours (Failed):      525,526,525,526,525,526,526,527,527,528,…
                                    ^^^      ^^^
```

The 524s in Mesen come from a get/put-loop interaction that the
parity formula can't capture: when DMC fires at a specific OAM-end
position, Mesen's loop produces 1 fewer cycle than mid-OAM DMC.
Our parity-only model uses `standalone - 1` uniformly.

**Fix path:** rewrite `Bus::run_oam_dma` as an explicit get/put
cycle loop matching Mesen2 NesCpu.cpp:399-447. The DMC service then
becomes a hijacker of get cycles inside that loop rather than a
separate stall. Estimated 1-2 days; touches the same surface as
phase 9's dropped refactor attempts. Branch off main, do incrementally,
keep `cpu_interrupts_v2/4-irq_and_dma` green throughout.

The standalone `sprdma_and_dmc_dma.nes` already passes with
identical-to-Mesen cycle counts, so the get/put rewrite is bounded
to the multi-DMC-fire-per-OAM corner case.

## 6. Cross-references

- [tools/trace_mesen.sh](../../tools/trace_mesen.sh), [tools/mesen_trace.lua](../../tools/mesen_trace.lua) — Mesen trace harness.
- [src/cpu/trace.rs](../../src/cpu/trace.rs) — our env-gated trace.
- [src/bus.rs `service_pending_dmc_dma`](../../src/bus.rs) — the parity-driven DMA service.
- [src/apu/dmc.rs `Dmc::new`](../../src/apu/dmc.rs) — DMC reset-tick offset.
- [tests/dmc_dma_during_read4.rs](../../tests/dmc_dma_during_read4.rs) — strict-pattern integration tests.
- `~/Git/Mesen2/Core/NES/NesCpu.cpp` lines 325-448 — `ProcessPendingDma` reference.
- `~/Git/Mesen2/Core/NES/APU/DeltaModulationChannel.cpp` lines 247-298 — DMC `_transferStartDelay` + `ProcessClock`.
- [notes/phase9/follow_ups.md](../phase9/follow_ups.md) — earlier dropped refactor attempts.
