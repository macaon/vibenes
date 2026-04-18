# 08.irq_timing.nes — root cause analysis

Test ROM: `~/Git/nes-test-roms/blargg_apu_2005.07.30/08.irq_timing.nes`
Source: `~/Git/nes-test-roms/blargg_apu_2005.07.30/source/08.irq_timing.asm`
Our result: fail code 2 ("Too soon"). Blargg says "IRQ handler is invoked
at minimum 29833 clocks after writing $00 to $4017".

The TL;DR: our frame IRQ flag is asserted one CPU cycle earlier than the
real 2A03. Test 07 (`6-irq_flag_timing.nes`) passes anyway because our
`$4015` read runs in `tick_pre_access`, which adds an accidental
one-cycle observation latency that hides the bug for polled reads.
IRQ-line dispatch does NOT share that latency, so test 08 exposes it.

---

## 1. When does our APU assert `frame_irq`?

Call site: `src/apu/frame_counter.rs:126–129`.

```
let mut event = step_event(self.region, self.mode, self.counter);
if event.set_frame_irq && self.irq_inhibit { event.set_frame_irq = false; }
```

`step_event` (same file, 149–167) returns `set_frame_irq=true` when
`counter` equals 29828, 29829, or 29830 (NTSC 4-step).

The `counter` field restarts at 0 on the cycle the pending `$4017`
write is applied (`frame_counter.rs:97–105`); subsequent calls
increment it every CPU cycle (`frame_counter.rs:124`). There is no
step event on the apply cycle itself — we early-return `FrameEvent::
default()`.

### Apply-cycle derivation

Write delay is computed in `frame_counter.rs:80–91` and `apu/mod.rs:
245–252`:

```
0x4017 => {
    let parity_odd = (self.cycle & 1) == 1;
    self.frame_counter.write_4017(data, self.cycle, parity_odd);
    …
}
```

Inside `write_4017`:

```
let delay = if parity_odd { 3 } else { 2 };
self.pending_write = Some(PendingWrite {
    value, apply_at: cycle.wrapping_add(delay),
});
```

Important subtlety: `self.cycle` inside `write_reg` is the APU's
completed-tick count. Because the APU ticks in `tick_post_access`
(after the bus access) — see `bus.rs:202–209` — at the moment
`write_reg` runs, `apu.cycle` is one less than
`bus.clock.cpu_cycles()`. So parity here is **the parity of the
pre-tick cycle index**, not of the master clock at the write cycle.
Blargg's `sync_apu` routine normalises this by rewriting `$4017`
until `$4015 bit 6` matches, so one consistent parity slot is
always selected before 07/08 run. The test passes reliably for
that selected parity; what follows assumes the "even" slot where
our `apply_at = apu_cycle + 2`.

### Fire cycle (even selected parity, bus cycle W)

Let `W` = bus master clock cycle on which the `STX $4017` write
executes (i.e., the post-access of this cycle ticks the APU for the
first time after the write). Then:
- `apu_cycle` inside `write_reg` = `W-1`.
- `apply_at = (W-1)+2 = W+1` (relative to apu_cycle timeline, i.e.,
  the tick when `cycle==W+1` inside `FrameCounter::tick` — but that
  tick happens during the bus cycle `W+2` because the APU runs one
  cycle behind the bus). In bus-absolute terms: apply happens during
  the post-access of bus cycle `W+2`.
- Counter=0 set during post-access of `W+2`. Counter=1 during
  post-access of `W+3`. … Counter=29828 during post-access of
  `W+2+29828 = W+29830`.

So `frame_irq = true` becomes true in the **post-access of bus
cycle W+29830** for the sync_apu-selected parity. The `irq_line`
flag in `bus.rs:205` is updated on that same post-access:

```
self.irq_line = self.apu.irq_line() | self.mapper.irq_line();
```

---

## 2. CPU–APU ordering around the fire cycle

Relevant files:
- `src/bus.rs:175–192` — `tick_pre_access` (PPU + NMI edge; snapshots
  `prev_irq_line` / `prev_nmi_pending`; does NOT tick APU).
- `src/bus.rs:202–209` — `tick_post_access` (ticks APU, refreshes
  `irq_line`).
- `src/cpu/mod.rs:126–150` — `step`, which calls `ops::execute`
  (containing all bus ticks) and then `poll_interrupts_at_end`.
- `src/cpu/mod.rs:155–193` — `poll_interrupts_at_end` reads
  `bus.prev_irq_line`.

### Cycle diagram (bus cycle F = first cycle we set `frame_irq`)

```
cycle F-1: pre(prev_irq=false) → access → post(tick; irq_line stays false)
cycle F:   pre(prev_irq=false) → access → post(tick; frame_irq=true; irq_line=true)
cycle F+1: pre(prev_irq=true)  → access → post(tick)   ← CPU now sees true
cycle F+2: pre(prev_irq=true)  → access → post(tick)
```

Poll semantics: an instruction whose last bus cycle is `L` reads
`bus.prev_irq_line` at the end of `ops::execute`. That value was
captured at `pre_access` of cycle `L`, i.e., reflects `irq_line` at
end of cycle `L-1`. Dispatch therefore triggers when `L-1 >= F`,
i.e., `L >= F+1`.

Given our `F = W+29830`, the earliest dispatching instruction has
`L = W+29831`.

### How `$4015` read fits in

`read_status` (`src/apu/mod.rs:177–203`) is called from the match
arm in `bus.read` at `src/bus.rs:106`. That match runs **between**
`tick_pre_access` and `tick_post_access` on the read cycle. Because
the APU ticks in `tick_post_access`, the `self.frame_irq` field
`read_status` observes is the value produced by last cycle's tick
— i.e., state at end of (read_cycle − 1).

So for a `$4015` bus read on cycle `R`:
- Observes `frame_irq = true` iff `R-1 >= F`, i.e., `R >= F+1`.

With our `F = W+29830`, first SET observed at `R = W+29831`.

That exactly matches blargg test 07's expectation of
"flag first set 29831 clocks after write" — **but only because our
pre-access read observation introduces an accidental +1-cycle
latency that compensates for `F` being 1 cycle early**.

---

## 3. Reference emulators

### Mesen2 — `~/Git/Mesen2/Core/NES/APU/`

- `ApuFrameCounter.h:19` NTSC step table: `{7457, 14913, 22371,
  29828, 29829, 29830}`.
- `ApuFrameCounter.h:99–145` — `Run()` fires step by step with
  `_previousCycle + cyclesToRun >= _stepCycles[mode][currentStep]`,
  then calls `SetIrqSource(IRQSource::FrameCounter)`
  (`ApuFrameCounter.h:110`) once the 4-step index reaches 3.
- `ApuFrameCounter.h:195–204` — write delay: `_writeDelayCounter =
  3` if even cycle, `4` if odd.
- `ApuFrameCounter.h:147–163` — decrement sequence: each `Run()`
  call decrements by 1 **at the end**, applies when it reaches 0.

Mesen orders events as:
1. `NesCpu::StartCpuCycle` (`NesCpu.cpp:317–323`) — increment cycle,
   call `ProcessCpuClock` → `Apu::Exec` → `Run` (catches frame
   counter up through `_currentCycle`, possibly firing the IRQ
   step).
2. Bus access (the read/write itself).
3. `NesCpu::EndCpuCycle` (`NesCpu.cpp:294–315`) — updates
   `_prevRunIrq = _runIrq; _runIrq = (IrqFlag & mask > 0 && !I)`.

Trace for even-parity `$00 → $4017` write at cycle `W`:
- `WriteRam` (`ApuFrameCounter.h:192–212`) sets
  `_writeDelayCounter = 3` during `W`.
- Decrements at end of Run on W+1 → 2, W+2 → 1, W+3 → 0 (apply).
  After apply, `_previousCycle = 0, _currentStep = 0`.
- Starting W+4, `_previousCycle` increments by 1 per Run. Step 3
  (value 29828) fires when the check `prev+1 >= 29828` passes:
  `prev = 29827`. That's at cycle `W+4+29827-1 + 1 = W+29831`.
  (Equivalently, `W + 3 + 29828 = W + 29831`.)

So Mesen: `IrqFlag` becomes true during cycle `W+29831`'s Run.
`_runIrq` becomes true at `EndCpuCycle(W+29831)`. `_prevRunIrq`
becomes true at `EndCpuCycle(W+29832)`. First dispatching
instruction has last cycle `W+29832`.

`$4015` read at `R`: Mesen's `ReadRam` (`NesApu.cpp:88–114`) calls
`Run()` which advances through `_currentCycle = R`. If `R ==
W+29831`, Run fires step 3 first, then `GetStatus` reads the freshly
set flag. First observed SET at `R = W+29831`. Same cycle as ours,
but reached from a different direction (no pre-access latency trick).

### Nestopia — `~/Git/nestopia/source/core/NstApu.cpp`

- `NstApu.cpp:38–55` — `frameClocks[3][4]` table; NTSC row is
  `{29830, 1, 1, 29830-2}`.
- `NstApu.cpp:2670–2706` `WriteFrameCtrl`: computes
  `next = cpu.Update(); if (cpu.IsOddCycle()) next += clock;
  Update(next); if (cycles.frameIrqClock <= next) ClockFrameIRQ(next);
  next += clock; cycles.frameCounter = …; cycles.frameIrqClock =
  next + frameClocks[0]`.
- `NstApu.cpp:2520–2537` — `ClockFrameIRQ` fires IRQ at
  `cycles.frameIrqClock`, then advances by table entries [1..3]
  (the `1,1,29830-2` sequence for 3 consecutive IRQ cycles).

Net effect for an even `$4017=$00` write at W: first
`frameIrqClock = W + 1 + 29830 = W + 29831`. For odd, `W + 2 +
29830 = W + 29832`. Matches Mesen within one parity's worth of
difference in how "current cycle" is indexed.

### puNES — `~/Git/puNES/src/core/apu.c`

- `apu.c:38–212` `apu_tick` — switch on `apu.step` 0..6; case 4
  (the 4-step-mode IRQ cycle) sets `nes[0].c.irq.high |= APU_IRQ`.
- `apu.h:502–518` `apuPeriod` table — NTSC 4-step delays:
  `{7459, 7456, 7458, 7457, 1, 1, 7457}`. The extra `1,1` between
  step 3 (the "set IRQ" step) and the wrap represents the 3-cycle
  IRQ-asserted window.
- `apu.h:249–280` write delay macros: `r4017.reset_frame_delay = 1`
  base + `+2` (4-step) or `+1` (5-step) additional — puNES counts
  from its own divider so the absolute delays are parity-corrected
  through `apuPeriod[...][0] = 7459` (where Mesen uses 7457).

Converges to the same "fires at `W + 29831/29832` for
even/odd" result as Mesen, just expressed against a different
reference origin.

### Bottom line across references

Real 2A03 / all three reference emulators: for an even-parity
write at `W`, frame IRQ becomes observable (via `$4015 bit 6` read
AND via the CPU's IRQ poll) **starting at cycle W+29831**. First
dispatching CPU instruction has its last bus cycle at `W+29832`.

Our emulator fires the APU flag one cycle earlier (at W+29830) but
disguises that fact for `$4015` reads via the pre-access read
latency. IRQ-line polling receives no such disguise and dispatches
at instruction-last-cycle `W+29831` — one cycle too soon, which is
exactly the "Too soon" (code 2) failure.

---

## 4. Best minimum-change fix hypothesis

### The bug is split across two mechanisms

(a) **APU-side**: `set_frame_irq` is triggered one cycle early in
absolute master-clock terms. Specifically our `apply_at = cycle +
{2 | 3}` at `frame_counter.rs:86` is 1 cycle shorter than Mesen's
equivalent `_writeDelayCounter = {3 | 4}` model.

(b) **Bus-side**: APU ticks in `tick_post_access`. That means
observation via `$4015` read sees a 1-cycle-stale APU state, while
observation via `bus.prev_irq_line` sees current-cycle state (as
far as end-of-penultimate-cycle polling expects). The two paths
have mismatched latencies.

(a) alone is the "real" bug per Mesen. (b) is compensating for it
for test 07, which is why only test 08 surfaces it.

### Pure APU-only fix: not sufficient

A straightforward `delay = if parity_odd { 4 } else { 3 }` change at
`frame_counter.rs:86` fixes (a) — frame IRQ fires at W+29831 — but
it **breaks test 07** because our pre-access `$4015` read now observes
the new state one cycle too late (first SET at R = W+29832 instead
of W+29831).

This mirrors the warning comment at `apu/mod.rs:164–176` which
already flags the tension:

> "Our bus ticks the APU at the end of every bus access […] in
> practice `read_status` observes the frame_irq state as of the
> previous cycle […] If a future test catches the race we must
> move the APU tick to the start of the bus op (or split it into
> pre/post halves)."

Test 08 IS that future test.

### Recommended fix: minimal bus edit + APU delay bump

Two small changes, both needed for parity with Mesen:

1. **`src/apu/frame_counter.rs:86`** — change the delay constants so
   the pending apply happens one cycle later:

   ```
   let delay = if parity_odd { 4 } else { 3 };
   ```

   Also bump the power-on pending write at `frame_counter.rs:54–57`
   from `apply_at: 3` to `apply_at: 4`. (Phase-5 comment about
   `count=5 vs count≈8` needs re-verifying under the new delay, but
   the direction is the same — we had previously tuned toward Mesen.)

2. **`src/bus.rs`** — move the APU tick (and the irq_line refresh)
   from `tick_post_access` to the end of `tick_pre_access`, keeping
   mapper tick and audio sampling in post-access. Concretely inside
   `tick_pre_access` (around line 190), after the PPU tick loop, add:

   ```
   self.apu.tick_cpu_cycle();
   self.irq_line = self.apu.irq_line() | self.mapper.irq_line();
   ```

   And remove those two lines from `tick_post_access`.

   Rationale: after this, a `$4015` read sees the frame counter's
   just-asserted flag on the cycle it fires (matches Mesen/hardware).
   CPU IRQ polling continues to use `prev_irq_line` captured at
   start-of-cycle — still semantically "end of penultimate".

   Caveat for **APU writes** on the same cycle as a half-frame
   clock: with this ordering, `length_clocked` is set *before* the
   write runs. That aligns with Mesen's behaviour (it runs APU at
   StartCpuCycle before the bus op). Length-load writes on a
   half-frame already test for this via `apu.length_clocked`
   (`apu/mod.rs:211, 216, 220, 224`) so the semantics stay correct.
   `$4003` / `$4007` / `$400B` / `$400F` writes should still suppress
   the reload on the clock cycle.

### If truly APU-only is required

Split `Apu::tick_cpu_cycle` into two parts:

- `tick_cpu_cycle_a(&mut self)` — runs `frame_counter.tick`,
  handles `set_frame_irq`, updates observable IRQ state. Called
  from `tick_pre_access`.
- `tick_cpu_cycle_b(&mut self)` — runs channel timers, DMA
  requests, audio sample emission. Called from `tick_post_access`.

This is still a bus edit but it's the most conservative one; the
interrupt-polling work from phase 5 doesn't depend on where `apu`
runs inside the bus cycle. The only regression risk is that the
`length_clocked` race has to be driven by phase A, which is fine.

Choose **option 1** (delay bump + APU tick in pre-access) unless
a test I haven't traced to surfaces a length-reload regression; in
that case fall back to the two-phase split.

---

## 5. Regression risk — other tests that exercise this path

| Test | Path it exercises | Why the fix is safe |
| ---- | ----------------- | ------------------- |
| `apu_test/3-irq_flag.nes` | Frame IRQ set + `$4015`-read clear logic, coarse timing. | Not cycle-precise enough to notice a 1-cycle shift; both current and fixed models assert the flag before the test samples. |
| `apu_test/6-irq_flag_timing.nes` (aka `07.irq_flag_timing`) | `$4015` read sees flag set at exactly W+29831. | CURRENTLY passes via accidental pre-access latency. After the fix, passes via the real mechanism (flag set at W+29831, read observes same cycle). Must be re-run: the new `delay=3/4` means `F` lands at W+29831 and the new APU-in-pre-access makes the `$4015` read see it on cycle W+29831. ✓ |
| `07.irq_flag_timing` (source-level test variant) | Same as above. | Same. |
| `cpu_interrupts_v2/1-cli_latency.nes` | CLI delaying IRQ by one instruction. | Does not depend on APU timing; drives `bus.apu.set_frame_irq_for_test` in unit tests. After bus shuffle, re-verify the `taken_no_cross_branch_delays_irq_by_one_instruction` unit test in `cpu/mod.rs:322–372` — the test uses `set_frame_irq_for_test` between steps, so it's timing-insensitive to APU-tick placement. |
| `cpu_interrupts_v2/2-nmi_and_brk.nes` | NMI hijack of BRK push phase; PPU-side timing. | No APU interaction. Bus pre/post split for PPU mid-cycle is untouched by this fix. |
| `cpu_interrupts_v2/3-nmi_and_irq.nes` | PPU NMI vs APU frame IRQ interaction. | CURRENTLY failing (noted in CLAUDE.md). This fix may actually help — test 3's symptom is "early NMI fires on odd iterations" and APU-frame-IRQ vs mid-cycle PPU tick ordering was listed as a suspect. Worth re-testing after the fix. |
| `apu_reset/*.nes` | Warm reset resets `$4017` state via `reset_on_cpu_reset`. | Reset path also goes through `write_4017` (`frame_counter.rs:74–78`), so the new delay applies. Confirm the "3/4 → 4/5" shift doesn't break `apu_reset/4017_timing` or `4017_written`. |
| `apu_test/4-jitter.nes` | Verifies `$4017` parity jitter. | This is the test `sync_apu` normalises against. Make sure parity computation in `apu/mod.rs:246` is still right — recall our parity uses `apu.cycle` which is one behind the bus cycle; after moving APU tick to pre-access, `apu.cycle` is ticked *before* `write_reg` runs, so `apu.cycle` at write_reg time equals the bus cycle. The parity rule may need to invert (current `parity_odd=true → delay 4` should probably become `parity_odd=true → delay 3` with the shifted timeline, or keep `4` and reverse the odd/even branches). Calibrate against `apu_test/4-jitter` specifically. |

### Regression-prevention checklist before committing

Run (from CLAUDE.md's "Do-before-starting checklist"):

```
cargo build --release && cargo test --lib --release
for rom in ~/Git/nes-test-roms/instr_test-v5/official_only.nes \
           ~/Git/nes-test-roms/instr_misc/instr_misc.nes \
           ~/Git/nes-test-roms/apu_test/rom_singles/*.nes \
           ~/Git/nes-test-roms/apu_reset/*.nes \
           ~/Git/nes-test-roms/cpu_interrupts_v2/rom_singles/*.nes \
           ~/Git/nes-test-roms/blargg_apu_2005.07.30/*.nes; do
  printf "%-40s " "$(basename $rom)"
  ./target/release/test_runner "$rom" 2>&1 | tail -1 | grep -oE 'PASS|FAIL'
done
```

Pay special attention to: `apu_test/4-jitter.nes` (parity),
`apu_test/5-len_timing.nes` (length-load race vs APU tick
ordering), `apu_reset/4017_timing.nes`, `apu_reset/4017_written.nes`,
and the full `blargg_apu_2005.07.30` set (tests 07, 08, 09, 10, 11
all depend on frame-counter timing).

---

## 6. Quick summary for the next implementer

- **Root cause**: `frame_irq` is asserted at cycle `W+29830` for
  the sync-selected parity; real hardware / Mesen assert at
  `W+29831`. Our `$4015` read hides the bug because it runs in
  pre-access (1-cycle stale view). IRQ-line polling runs against
  end-of-penultimate state, which is NOT stale, so test 08 fails.
- **Smallest effective fix**:
  1. `src/apu/frame_counter.rs:86` — `delay = if parity_odd { 4 }
     else { 3 }` (also bump the power-on `apply_at` on line 56
     from 3 to 4).
  2. `src/bus.rs:202–209` — move `self.apu.tick_cpu_cycle()` and
     the `self.irq_line` refresh from `tick_post_access` up to
     `tick_pre_access` (keep mapper tick + audio sink where they
     are).
  3. Verify parity sign in `apu/mod.rs:246` against
     `apu_test/4-jitter` — may need to flip branches now that
     `apu.cycle` equals the bus cycle at write time.
- **Do not** attempt to fix this purely inside `frame_counter.rs`.
  The asymmetry is between `$4015` read observation and IRQ-line
  polling — one of them must move relative to the APU tick, which
  lives in the bus.
