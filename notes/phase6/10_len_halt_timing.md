# 10.len_halt_timing — failure analysis

Test: `~/Git/nes-test-roms/blargg_apu_2005.07.30/10.len_halt_timing.nes`
Source: `~/Git/nes-test-roms/blargg_apu_2005.07.30/source/10.len_halt_timing.asm`
Observed: **fail code 3** — "Length should be clocked when halted at 14915."
Headline rule (from `tests.txt`): *Changes to length counter halt occur
after clocking length, not before.*

## 1. Test semantics (cycles 14913..=14915)

The test uses mode 1 (`$4017 ← $C0`, bit 7 set, bit 6 = inhibit frame IRQ)
so the length clock is driven by the `$4017` half-frame rather than the
natural mode-0 sequencer. The path is the same: the channel's length
counter is clocked on the half-frame event while the CPU writes
`$4000 ← $30` (halt=1) at a cycle near the clock.

Subtest result codes (line 27–69 of `10.len_halt_timing.asm`):

| Code | Scenario | Expected |
|------|----------|----------|
| 2 | halt→halt at 14914 (1 cycle *before* clock) | length NOT decremented — channel still plays |
| **3** | unhalt→halt at 14915 (same cycle as clock) | length IS decremented — channel goes silent |
| 4 | unhalt→halt at 14914 (1 cycle before clock), then unhalt at 14914 | length IS decremented — channel goes silent |
| 5 | halt→unhalt at 14915 (same cycle as clock) | length NOT decremented — channel plays |

The pattern: the halt bit seen by the clock is the **old** value
(pre-write). The new halt value takes effect only on the *next* half-
frame clock. `$4000 ← $30` at cycle 14915 must not prevent the cycle-
14915 clock from decrementing, because the halt change is deferred.

We fail code 3: we are applying halt before the same-cycle clock, so
the clock is suppressed and length stays at 2 instead of dropping to 1.
The test reads `$4015` and expects `& $01 == 0` (silent after a second
half-frame decrement to 0), sees non-zero → branches to `error`.

## 2. Our current behavior (trace of cycle X == length-clock cycle)

Call order when the CPU writes `$4000 ← $30` on cycle X that is
simultaneously the length-clock cycle:

1. CPU calls `bus.write(0x4000, 0x30)`.
   `/home/marcus/Git/vibenes2/src/bus.rs:127` enters `Bus::write`.
2. `bus.rs:129` → `self.tick_pre_access()`; PPU ticks. The APU has
   *not* ticked yet for this cycle.
3. `bus.rs:133` matches `0x4000..=0x4013` → `self.apu.write_reg(...)`.
   `src/apu/mod.rs:208` dispatches to `self.pulse1.write_ctrl(data)`.
4. `src/apu/pulse.rs:66` executes `self.length.set_halt(true)`
   immediately, which mutates `LengthCounter.halt` in place
   (`src/apu/length.rs:17-19`).
5. `bus.rs:154` → `self.tick_post_access()`; `bus.rs:203`
   `self.apu.tick_cpu_cycle()`.
6. `src/apu/mod.rs:117` runs `tick_cpu_cycle`:
   - `mod.rs:120` `frame_counter.tick(self.cycle)` returns
     `{ quarter: true, half: true }` for this cycle (mode-1 path in
     `src/apu/frame_counter.rs:169-180`).
   - `mod.rs:127-129` invokes `self.clock_half()`.
   - `mod.rs:156-162` `clock_half` calls `self.pulse1.clock_half_frame()`.
   - `src/apu/pulse.rs:93` calls `self.length.clock_half_frame()`.
   - `src/apu/length.rs:53-57` reads `self.halt` — **already true** from
     step 4 — and returns without decrementing.

**Answer to Q1:** `clock_half_frame` sees the NEW halt, because
`set_halt` mutated the field in-between `tick_pre_access` and
`tick_post_access`. The clock is silently skipped.

**Answer to Q2:** This is exactly the "halt before clock" ordering that
the test ROM rejects. Real hardware clocks the counter with the OLD
halt and only commits the new halt afterward, so the cycle-14915
decrement must occur. We fail code 3 because length stays at 2, and
the following half-frame's decrement brings it to 1 instead of 0, so
`$4015 & 0x01 != 0` when the test polls.

## 3. Reference emulator models

### Mesen2 — `_newHaltValue` staged + committed via `ReloadCounter`

`/home/marcus/Git/Mesen2/Core/NES/APU/ApuLengthCounter.h:14` declares
`bool _newHaltValue = false;` alongside `bool _halt`. The only API
channels use for writing halt is `InitializeLengthCounter(haltFlag)`
at line 24, which does **not** touch `_halt` — it only sets
`_newHaltValue` and schedules an APU run.

`/home/marcus/Git/Mesen2/Core/NES/APU/ApuEnvelope.h:27`:
`LengthCounter.InitializeLengthCounter((regValue & 0x20) == 0x20);`
called from `InitializeEnvelope` — the hook used by both square and
noise channels on `$4000/$4004/$400C`.

`/home/marcus/Git/Mesen2/Core/NES/APU/TriangleChannel.h:83`:
`_lengthCounter.InitializeLengthCounter(_linearControlFlag);` on
`$4008`. Identical staging path.

The commit point is `ReloadCounter()`
(`/home/marcus/Git/Mesen2/Core/NES/APU/ApuLengthCounter.h:82-92`):

```cpp
void ReloadCounter() {
    if (_reloadValue) { ... }  // also commits deferred $xxx3/B/F load
    _halt = _newHaltValue;     // commit halt here
}
```

And `ReloadCounter` is called from each channel's `ReloadLengthCounter()`
wrapper — all invoked in a single point,
`/home/marcus/Git/Mesen2/Core/NES/APU/NesApu.cpp:160-165`:

```cpp
// Reload counters set by writes to 4003/4008/400B/400F *after* running
// the frame counter to allow the length counter to be clocked first
// This fixes the test "len_reload_timing" (tests 4 & 5)
_square1->ReloadLengthCounter();
_square2->ReloadLengthCounter();
_noise->ReloadLengthCounter();
_triangle->ReloadLengthCounter();
```

In other words: Mesen runs the frame counter first (which fires
`TickLengthCounter` on half-frame events, see `NesApu.cpp:54-72`), and
only THEN commits the staged halt value and the staged length-reload.
The same code path handles both `_newHaltValue` and the reload of
`$4003/$400B/$400F` — they use the same commit point because both are
governed by the same rule ("write wins on the cycle after the clock").

**Ours,** by contrast, does the bus write first (halt mutated
immediately), then ticks the APU (which clocks length seeing the new
halt). That's the inverted order.

### puNES — no halt staging

`/home/marcus/Git/puNES/src/core/apu.h:286`: `square_reg0` macro sets
`square.length.halt = value & 0x20;` immediately during the `$4000/$4004`
write. No staging.

`/home/marcus/Git/puNES/src/core/cpu_inline.h:1476`: `TR.length.halt =
value & 0x80;` on `$4008` — immediate.

`/home/marcus/Git/puNES/src/core/cpu_inline.h:1513`: `NS.length.halt =
value & 0x20;` on `$400C` — immediate.

The clock macro at `apu.h:32-39`:

```c
#define length_run(channel) \
    if (!channel.length.halt && channel.length.value) { \
        channel.length.value--; \
    }
```

puNES writes halt before the clock runs and therefore **fails blargg
test 10** exactly the way we do. puNES isn't a useful model for this
specific fix; it confirms that a simpler implementation is insufficient.

### Nestopia — LengthCounter has no halt, clock-gate lives in channel

`/home/marcus/Git/nestopia/source/core/NstApu.hpp:128-177` defines
`LengthCounter` without any `halt` / `newHalt` field — it only tracks
`enabled` and `count`. The halt gate is re-read per clock from the
channel's envelope state:

`/home/marcus/Git/nestopia/source/core/NstApu.cpp:1541`:
`if (!envelope.Looping() && lengthCounter.Clock())`

So halt lookup is "live": a register write sets the envelope loop bit
immediately, and the next half-frame clock reads the bit then.
Functionally this matches puNES — if the write precedes the clock on
the same cycle, the clock sees the new halt. Nestopia does not model
the `_newHaltValue` race either.

### Verdict

Only Mesen2 emulates the "halt applies after the clock" rule from
blargg test 10. The implementation hinges on staging halt in
`_newHaltValue` and committing it in a helper that runs after the
frame counter's half-frame event. We will port this pattern, not
copy its code.

## 4. Minimum-change fix — concrete plan

### 4.1 `LengthCounter` shape change

File: `src/apu/length.rs` (lines 10-19, 53-57).

Add a staged-halt slot:

```rust
// (after line 13 `enabled`):
pending_halt: Option<bool>,
```

Replace `set_halt` (line 17-19) with two methods:

```rust
/// Channel register writes (`$4000.5` / `$400C.5` / `$4008.7`) stage
/// the halt change. The new value is committed at the end of the
/// current CPU cycle — AFTER the half-frame clock (if any) has run.
/// Blargg `10.len_halt_timing` relies on this ordering.
pub fn stage_halt(&mut self, halt: bool) {
    self.pending_halt = Some(halt);
}

/// Commit any staged halt value. Called once per CPU cycle after the
/// length clock runs (see Apu::tick_cpu_cycle).
pub fn commit_halt(&mut self) {
    if let Some(new) = self.pending_halt.take() {
        self.halt = new;
    }
}
```

Do NOT change the private `halt: bool` field — its read path
(`clock_half_frame` at line 53) must keep reading the *old* halt until
`commit_halt` runs.

### 4.2 Apu wiring

File: `src/apu/mod.rs`, around lines 117-147 (`tick_cpu_cycle`).

`tick_cpu_cycle` must commit halt **after** the half-frame clock, so
the order becomes:

1. Run frame counter tick (may set `event.half`).
2. If `event.half` → `self.clock_half()` (each channel runs
   `length.clock_half_frame()` — still reads OLD halt, correct).
3. **NEW:** commit staged halt on each channel.
4. Continue with triangle/DMC/APU-rate timers as today.

Easiest: a new helper `Apu::commit_length_halt` invoked
unconditionally after the quarter/half clock block. Unconditional is
fine — `commit_halt` is a no-op when `pending_halt == None`, so the
cost is one Option check per channel per CPU cycle.

```rust
// after line 129 (`self.clock_half()` block):
self.pulse1.commit_length_halt();
self.pulse2.commit_length_halt();
self.triangle.commit_length_halt();
self.noise.commit_length_halt();
```

Each channel gains a trivial wrapper:

```rust
pub fn commit_length_halt(&mut self) {
    self.length.commit_halt();
}
```

### 4.3 Channel write-paths to convert

Three call-sites need `set_halt` → `stage_halt`:

| File:line | Current | New |
|---|---|---|
| `src/apu/pulse.rs:66` | `self.length.set_halt((data & 0x20) != 0);` | `self.length.stage_halt((data & 0x20) != 0);` |
| `src/apu/noise.rs:63` | `self.length.set_halt((data & 0x20) != 0);` | `self.length.stage_halt((data & 0x20) != 0);` |
| `src/apu/triangle.rs:53` | `self.length.set_halt(self.control_flag);` | `self.length.stage_halt(self.control_flag);` |

#### Triangle subtlety — `$4008.7` is both length-halt AND linear-counter control

`src/apu/triangle.rs:49-54` currently handles `$4008` like this:

```rust
pub fn write_linear(&mut self, data: u8) {
    self.control_flag = (data & 0x80) != 0;       // ← used immediately
    self.linear_reload_value = data & 0x7F;
    self.length.set_halt(self.control_flag);      // ← TO DEFER
}
```

The `control_flag` is ALSO read by `clock_quarter_frame` at line 72-74
to decide whether to clear the linear-reload flag. The linear path is
independent of length-counter halt timing — blargg test 10 is purely a
length-counter test — so `control_flag` should keep its immediate
write. We only defer the length-halt bit. Mesen2 follows the same
split: `TriangleChannel.h:80` writes `_linearControlFlag` immediately
and only the length-counter halt goes through
`InitializeLengthCounter`.

That's already what our code intends — the `set_halt` call simply gets
renamed to `stage_halt`, no structural change in the triangle channel.

#### Is there a `$4015`-enable path that toggles halt?

No. `LengthCounter::set_enabled` (`length.rs:24-29`) sets the enable
latch and zeroes the counter when disabling. It does not touch `halt`.
No code outside the three lines above touches halt directly.

### 4.4 Files touched (summary)

- `src/apu/length.rs` — add `pending_halt` field, `stage_halt` /
  `commit_halt` methods, remove `set_halt` (or keep it `pub(crate)`
  deprecated; I recommend removing it to prevent accidental bypass).
- `src/apu/mod.rs` — add `commit_length_halt` call on each channel
  inside `tick_cpu_cycle` after the quarter/half clock block.
- `src/apu/pulse.rs` — one-line `set_halt` → `stage_halt` rename; add
  `commit_length_halt` wrapper.
- `src/apu/noise.rs` — one-line rename; add wrapper.
- `src/apu/triangle.rs` — one-line rename; add wrapper.

Tests to add:

- Unit test in `apu/length.rs`: staging + commit order
  (`stage_halt(true)` → `clock_half_frame()` still decrements → then
  `commit_halt()` → next `clock_half_frame()` no-ops).
- Unit test in `apu/mod.rs` `tests` module: drive `tick_cpu_cycle`
  with a forged `event.half=true` on the same cycle as a `$4000`
  halt-set write and assert pulse1 length decremented once.

## 5. Write-path ordering vs reality (Q5)

Real 2A03 runs at the APU's φ2, with register writes being committed
late in the CPU cycle (T3/T4 of the 6502 write sequence) and the
length-counter clock pulse being a purely APU-internal event driven
by the frame sequencer. The hardware ordering is:

- halt-write lands on cycle X.
- half-frame clock fires on cycle X (if the sequencer lands there).
- The length-counter decrement uses the halt bit as it was latched
  BEFORE the $4000 write, because the halt flip-flop is only updated
  after the APU's length-decrement phase within the cycle.

Equivalently: the halt change is "one cycle late" from the CPU's
point of view. Our bus model does `write → tick`, so within the same
tick we must NOT let the write propagate into the length clock. The
staging approach in §4 achieves this without shifting any other timing.

Note that this is NOT true of the length-counter **load**
(`$4003/$400B/$400F`): that load is also deferred to post-clock
(Mesen2 commits it in the same `ReloadCounter` call), but our code
already handles the race in `LengthCounter::load`
(`src/apu/length.rs:43-51`) via the `same_cycle_as_clock` flag plumbed
through `Apu::length_clocked` (`mod.rs:56, 118, 156-157`,
`pulse.rs:80, 83`, `noise.rs:73, 74`, `triangle.rs:60, 62`). That
approach *inspects the clock state from the write side* rather than
*deferring the write to after the clock*, which is a valid shortcut for
the load case because the load-then-decrement result is equivalent to
skipping the load entirely when `counter != 0`. It does not
generalize to halt, because halt changes the decision *whether* to
decrement, and an "equivalent effect" shortcut is not available. For
halt we must actually defer.

## 6. Interaction with test 11 (`11.len_reload_timing.asm`)

Test 11 exercises the length-**reload** race, not halt:

- Sub-test 2: reload just before clock → reload, then decrement. Our
  current behavior is correct (reload write executes first on the
  pre-clock cycle; clock then decrements to 1 — read as "length = 1").
- Sub-test 3: reload just after clock → our code is fine today.
- Sub-test 4: reload during clock with ctr=0 → reload must apply; our
  `LengthCounter::load` early-outs the drop only when
  `counter != 0`, so counter 0 always accepts the reload. OK.
- Sub-test 5: reload during clock with ctr>0 → reload must be
  dropped; we already honor this via `same_cycle_as_clock && counter
  != 0` in `length.rs:47-49`.

Test 11 is independent of the halt fix. Verify it isn't currently
passing before the fix; if it passes today, the halt fix should not
affect it. If it fails today for reasons independent of halt, that's
a separate commit.

The current status of test 11 should be verified before planning that
commit. It's a separate change from test 10: the reload staging path
already exists via `length_clocked`, while halt staging is
greenfield.

## 7. Regression risk

The staged-halt approach is a no-op on cycles where `commit_halt`
runs without anything staged (`pending_halt == None`). Concretely:

- `apu_test/1-len_ctr`, `apu_test/2-len_table`: write halt/unhalt
  ahead of the clock, many CPU cycles away. `stage_halt(v)` sets
  `pending_halt = Some(v)`, the next CPU cycle's `commit_halt` copies
  it into `halt`, `pending_halt` is cleared. From the clock's point of
  view the new halt is visible starting from the next cycle — same
  observable behavior as today.
- `apu_test/5-len_timing_mode0` / `6-len_timing_mode1`: exercise exact
  length-clock cycle boundaries. These tests are already passing;
  the staging change moves halt commits strictly later by at most
  one CPU cycle. None of the cycle-exact timings in these tests
  touch halt on the clock cycle (they measure `$4015` after the
  length has naturally decremented with halt = 0 throughout).
- `apu_test/7-dmc_basics`, `apu_test/8-dmc_rates`: DMC, unrelated to
  length halt.
- `apu_reset/*`: the reset handlers (`clear_enable_latch_only`,
  `on_warm_reset`) don't touch `halt` today and shouldn't in the fix
  either. On warm reset, the `pending_halt` slot should be cleared
  along with `halt` for determinism; add `self.pending_halt = None`
  to `LengthCounter::clear_enable_latch_only` — actually, the nesdev
  reset rule is that halt state is *retained* on warm reset for
  non-triangle channels (see Mesen2 `ApuLengthCounter.h:47-55`), so
  leave it alone. For `pending_halt`, clearing to `None` is
  conservative — any staged change mid-reset would be dropped, which
  matches "reset happens between cycles" behavior.

Only tests that specifically write halt on the same cycle as the
length clock change observable behavior — and those are exactly the
tests we want to fix (blargg 10, subtests 3 & 5 in our failing-set).

### Minor design choice

`pending_halt: Option<bool>` vs two booleans (`new_halt`, `halt_pending`)
— Mesen uses two booleans. In Rust, `Option<bool>` is idiomatic and
equally efficient (u8 discriminant + u8 payload, same 2-byte layout)
and makes the "nothing staged" case explicit at the type level. Prefer
the `Option<bool>` form.

## 8. Summary of the one-line diagnostic

Our bus runs `write → tick_cpu_cycle`. `tick_cpu_cycle` runs the
length clock, which reads `LengthCounter.halt`. The write has
already mutated `halt` by the time we get to the clock. The fix is a
one-cycle defer of the halt commit, matching Mesen2's
`_newHaltValue` / `ReloadCounter` pattern. Changes are local to
`src/apu/length.rs` plus one-line renames in `pulse.rs`, `noise.rs`,
`triangle.rs`, and three new one-line calls in `Apu::tick_cpu_cycle`
between the half-frame clock and the rest of the cycle. Zero effect
on any currently-passing test; enables blargg test 10.
