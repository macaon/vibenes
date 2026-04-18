# blargg `11.len_reload_timing` — phase-6 investigation

Test ROM: `~/Git/nes-test-roms/blargg_apu_2005.07.30/11.len_reload_timing.nes`
Source: `~/Git/nes-test-roms/blargg_apu_2005.07.30/source/11.len_reload_timing.asm`
Failing result code: **3** ("Reload just after length clock should work normally").

Test result code legend:
| Code | Case                                                           |
|------|----------------------------------------------------------------|
| 1    | All four sub-tests pass                                        |
| 2    | Reload **just before** clock — should work                     |
| 3    | Reload **just after** clock — should work     ← we fail here  |
| 4    | Reload **during** clock when ctr == 0 — should work            |
| 5    | Reload **during** clock when ctr > 0 — should be IGNORED       |

## 1. What blargg measures

`source/11.len_reload_timing.asm:30-45` — the "just after" case:

```
jsr   sync_apu                ; align to frame-counter cycle 0
lda   #$38                    ; length index $07 (= 6)
sta   reload                  ; $4003
lda   #$40
sta   $4017                   ; 4-step, IRQ inhibit
ldy   #244                    ; delay 14910 CPU cycles
lda   #11
jsr   delay_ya9
lda   #$18                    ; length index $03 (= 2)
sta   reload                  ; "write at 14915" (comment in asm)
```

Compared to the "just before" case (line 13-28), the delay differs by **2 CPU
cycles** (244/lda#11/delay_ya7 vs delay_ya9), placing the second `sta $4003`
one CPU cycle on either side of the half-frame clock that fires at counter
14913 (NTSC 4-step sequencer, see `src/apu/frame_counter.rs:155-158`).

The hardware behaviour:
- **Just before**: `length_clocked` (our flag; hardware equivalent: the
  half-frame pulse) is low at the moment of the register strobe → load runs.
- **Just after**: `length_clocked` was high *one cycle ago* but is low now
  → load runs.
- **Same cycle as clock**: both events collide on one CPU cycle. Load is
  silently dropped when counter != 0 (the classic blargg length-race rule).

The rule therefore needs exact same-cycle coincidence detection. Any window
wider than one cycle will leak into either the "just before" or "just after"
case.

## 2. Current emulator walkthrough

Files in play:
- `src/bus.rs:127-155`  — CPU write path; APU write happens **between**
  `tick_pre_access` and `tick_post_access`.
- `src/apu/mod.rs:117-147` — `Apu::tick_cpu_cycle`. Clears
  `self.length_clocked = false` at entry (line 118), then runs the frame
  counter and, if `event.half`, calls `clock_half()` which sets
  `self.length_clocked = true` at line 157.
- `src/apu/mod.rs:211,216,220,224` — `write_reg` calls each channel's
  `write_*` with `self.length_clocked` as the "same-cycle" flag.
- `src/apu/length.rs:43-51` — `LengthCounter::load` drops the reload when
  `same_cycle_as_clock && counter != 0`.

Timeline walk-through for the event cycle `C = 14913` (half-frame clock).
Let `S(x)` denote "state of `length_clocked` at point `x`".

| time (inside `bus.write(0x4003,…)`)               | `length_clocked` state |
|---|---|
| entering cycle C, before anything                  | cleared at end of C-1 tick → **false** |
| `tick_pre_access` (PPU tick + NMI latch)           | **false** (APU untouched) |
| `apu.write_reg(0x4003, d)` → `pulse.write_timer_hi(d, length_clocked=false)` | **false** (wrong! should be true) |
| `tick_post_access` → `apu.tick_cpu_cycle()`        |                       |
|  └── `length_clocked = false` (line 118, no-op)    | **false**             |
|  └── `frame_counter.tick()` returns `event.half=true` |                    |
|  └── `clock_half()` sets `length_clocked = true`   | **true**              |
| end of cycle C                                     | **true**              |

Now cycle `C+1` (what blargg's "just after" exercises):

| time (inside `bus.write(0x4003,…)` at C+1)         | `length_clocked` state |
|---|---|
| entering cycle C+1                                 | **true** (stale from C!) |
| `tick_pre_access`                                  | **true**              |
| `apu.write_reg(0x4003, d)` → `pulse.write_timer_hi(d, length_clocked=true)` | **true** (wrong! should be false) |
| `tick_post_access` → `apu.tick_cpu_cycle()`        |                       |
|  └── `length_clocked = false` (line 118)           | **false**             |
|  └── `frame_counter.tick()` — no event             |                       |
| end of cycle C+1                                   | **false**             |

So the flag is **shifted by one CPU cycle**. On the actual event cycle it
reads false (load succeeds — wrong → test 5 fails), and on the cycle after
it reads stale true (load dropped — wrong → test 3 fails).

This is the **exact** failure mode described in the prompt and matches the
observed result code 3. (Test 5 presumably also fails in isolation, but the
test ROM aborts at the first mismatch, so we only see code 3.)

## 3. Why it's one cycle off — root cause

The bus schedules the APU register write **before** `apu.tick_cpu_cycle()`.
That was the right choice for most registers (writes visible to the APU on
the same cycle they happen), but it means the `length_clocked` flag we hand
to the channel is whatever the PREVIOUS cycle's APU tick left behind. That
previous-cycle state is exactly one cycle late for length-race detection.

Put differently: our flag answers the question *"did a length clock occur
at the end of the last CPU cycle?"*, not *"is a length clock firing this
CPU cycle?"* — which is what the blargg rule requires.

## 4. Reference emulators — how they solve it

### 4.1 Mesen2 — deferred reload (approach *b*)

`Core/NES/APU/ApuLengthCounter.h:14,20,30-37`:
```
bool _newHaltValue = false;       // pending halt
uint8_t _reloadValue = 0;         // pending reload
uint8_t _previousValue = 0;       // counter at moment of write

void LoadLengthCounter(uint8_t value) {
  if(_enabled) {
    _reloadValue = _lcLookupTable[value];
    _previousValue = _counter;
    _console->GetApu()->SetNeedToRun();
  }
}
```

The write merely **stages** the reload. The glitch rule is enforced later
in `ReloadCounter()` (`ApuLengthCounter.h:82-92`):
```
void ReloadCounter() {
  if(_reloadValue) {
    if(_counter == _previousValue) {   // counter hasn't moved since write
      _counter = _reloadValue;
    }
    _reloadValue = 0;
  }
  _halt = _newHaltValue;                // test 10: halt also deferred
}
```

`ReloadCounter()` is called from `NesApu::Run()` (`NesApu.cpp:162-165`)
*after* `_frameCounter->Run(cyclesToRun)` has had a chance to tick the
length counter for the cycle slice:
```
_previousCycle += _frameCounter->Run(cyclesToRun);

// Reload counters set by writes to 4003/4008/400B/400F after running
// the frame counter to allow the length counter to be clocked first.
// This fixes the test "len_reload_timing" (tests 4 & 5)
_square1->ReloadLengthCounter();
_square2->ReloadLengthCounter();
_noise->ReloadLengthCounter();
_triangle->ReloadLengthCounter();
```

The `_counter == _previousValue` predicate is Mesen2's clever trick: if the
half-frame clock fired between the write and the reload, the counter is
one lower than it was at write time → the predicate fails → the reload is
suppressed. If the counter was zero at write time, `_previousValue = 0`,
and after a clock tick it stays at 0 (can't decrement below zero), so the
predicate still holds → reload runs (test 4 path). This single check covers
both codes 4 and 5 and is insensitive to the exact cycle the write lands
on within the run slice.

### 4.2 puNES — `apu.length_clocked` predicate (approach *a*, done right)

`src/core/apu.h:40-45` — the half-frame macro **sets** the flag:
```
#define length_clock()\
    apu.length_clocked = TRUE;\
    length_run(S1)\
    length_run(S2)\
    length_run(TR)\
    length_run(NS)\
    …
```

`src/core/apu.c:45` — `apu_tick` (called every hwtick at the START of each
CPU cycle) **clears** the flag:
```
apu.length_clocked = FALSE;
```

`src/core/apu.h:297-320` — `square_reg3` (the $4003/$4007 write) reads the
flag AT THE MOMENT OF THE WRITE:
```
if (square.length.enabled && !(apu.length_clocked && square.length.value)) {
    square.length.value = length_table[value >> 3];
}
```

Crucially, **the ordering of `tick_hw` vs `apu_wr_reg` is different from
ours**. `src/core/cpu_inline.h:961-971`:
```
if (address == 0x4015) {
    apu_wr_reg(nidx, address, value);
    tick_hw(nidx, 1);                // $4015: write first, then tick
    return;
}
if (address <= 0x4017) {
    tick_hw(nidx, 1);                // $4000-$4014, $4017: tick first
    apu_wr_reg(nidx, address, value);
    return;
}
```

For the length-register writes ($4003/$4007/$400B/$400F), **`tick_hw` runs
first, meaning `apu_tick` clears and then possibly sets `length_clocked`
before the write reads it**. So on the event cycle `length_clocked` is
observed TRUE, and on the following cycle it is observed FALSE. That is
the inverse of our situation.

### 4.3 Nestopia — write-time delta check (approach *a*, variant)

`source/core/NstApu.hpp:160-166`:
```
void Write(uint data, bool frameCounterDelta) {
    NST_VERIFY_MSG( frameCounterDelta, "APU $40xx/framecounter conflict" );
    if (frameCounterDelta || !count)
        Write( data );
}
```

`source/core/NstApu.cpp:771-777` — `UpdateDelta` computes whether the
frame counter is EXACTLY at the current cycle:
```
bool Apu::UpdateDelta() {
    const Cycle elapsed = cpu.Update();
    const bool delta = cycles.frameCounter != elapsed * cycles.fixed;
    Update( elapsed + 1 );
    return delta;
}
```

`delta` is true when the cycle boundaries differ — i.e. when we're NOT on
a frame-counter event cycle. If delta is true, or counter is zero, write
proceeds. Otherwise it is dropped. Same rule as blargg, evaluated at write
time using a live comparison rather than a cached flag.

## 5. Answers to the seven questions

1. **When does `length_clocked` clear?** Line `src/apu/mod.rs:118`
   (first statement of `tick_cpu_cycle`). Because `tick_cpu_cycle` is
   called from `tick_post_access` (`src/bus.rs:203`), which runs *after*
   the register write dispatch, the register write sees the flag value
   left over from the previous cycle's APU tick.

2. **Why we fail code 3.** On the cycle immediately after the half-frame
   clock fired, `length_clocked` is still TRUE because it was set by
   `clock_half()` at line 157 of mod.rs and has not yet been cleared (the
   clear only runs at the top of the next `tick_cpu_cycle`, which in turn
   only runs inside `tick_post_access`, which comes *after* the register
   write on this new cycle). The channel's `load` therefore drops a
   reload that the hardware would accept.

3. **Reference model.**
   - Mesen2: stage the reload on write; apply it later from
     `NesApu::Run` after the frame counter has had its turn. Predicate
     `_counter == _previousValue` neatly distinguishes "clock hasn't
     moved things" from "clock fired between write and apply".
   - puNES: a `length_clocked` pulse flag, BUT the `tick_hw`
     (apu_tick → possibly length_clock) runs *before* `apu_wr_reg` for
     $4000-$4014/$4017. Therefore within one CPU cycle the flag is
     clear-then-possibly-set before the write observes it. The flag is
     effectively a "this-cycle's half-frame" predicate.
   - Nestopia: compute "frame counter is exactly at current cycle" at
     write time, pass as `frameCounterDelta` into `LengthCounter::Write`.

4. **Fix plan — two candidates.**

   **(a) "This-cycle is half-frame" predicate.** At the start of
   `tick_cpu_cycle` (before the write in the bus), peek the frame counter
   to decide whether `event.half` will fire, cache it in a field like
   `apu.this_cycle_is_half_frame`, and have the channel `load` consult
   that instead of the post-facto `length_clocked` flag.

   Drawback: requires pre-computing `step_event(region, mode, counter+1)`
   from the bus (*before* the register write), which forces us to split
   `FrameCounter::tick` into `peek_event_for_next_cycle` + `advance`.
   The pending-$4017-write logic (`src/apu/frame_counter.rs:95-117`)
   also mutates mode/counter at tick time, so the peek must replicate
   that path. Workable, but increases the surface area and couples the
   frame-counter internals to the bus ordering.

   **(b) Deferred reload (Mesen2 style).** On $4003/$4007/$400B/$400F
   write, don't modify the counter. Stash
   `{reload_index, counter_at_write}` in the channel. After the APU
   tick at the end of the cycle (or on entry to the next
   `tick_cpu_cycle`), if a pending reload exists, apply it using the
   same rule as Mesen2: if the counter hasn't decremented since the
   write, load; otherwise drop.

   Drawback: one extra bit of per-channel state, plus a
   `commit_pending_reload()` hook. On the other hand:
   - no intrusive changes to the frame counter or bus;
   - naturally composes with test-10's pending-halt deferral
     (Mesen2's `_newHaltValue` shares the same apply slot — see
     `ApuLengthCounter.h:91`);
   - matches the operational model the hardware is known to have (all
     length-counter decisions happen at the APU's rising edge, not at
     the CPU's write instant);
   - the `_counter == _previousValue` predicate is a one-line test
     that encodes both the "ctr == 0" (code 4) and "ctr > 0" (code 5)
     branches of the blargg rule without any further flag fiddling.

   **Recommendation: approach (b).** Simpler to reason about, fewer
   coupling points, and it is exactly the path Mesen2 calls out with
   the comment `// This fixes the test "len_reload_timing"
   (tests 4 & 5)` (`NesApu.cpp:160-161`). Approach (b) also generalises
   to MMC5 expansion audio (Mesen2 reuses `ReloadLengthCounter` there,
   `Mmc5Audio.h:89-90`), which we will benefit from when phase 7 lands.

5. **Is `length_clocked` broken?**

   Yes — *as observed from the write path*. Audit:
   - `length_clocked = false` at `mod.rs:118` (start of
     `tick_cpu_cycle`).
   - `length_clocked = true` at `mod.rs:157` (inside `clock_half`,
     called from the same `tick_cpu_cycle`).

   Because `tick_cpu_cycle` runs in `tick_post_access` (`bus.rs:203`)
   AFTER the register write, any write observes the flag from the
   PREVIOUS cycle. The flag does indeed remain `true` across the cycle
   boundary: it is set at the end of the clock cycle and only cleared
   at the start of the next cycle's tick — which is after the next
   cycle's register write. Net effect: the flag is one CPU cycle later
   than the event it is supposed to represent. This is a correctness
   bug; the flag works for code paths that consult it *inside*
   `tick_cpu_cycle` (none today), but not for anything that consults it
   from the CPU write ordering.

6. **Interaction with test 10.**

   Test 10 is the pending-halt fix (`$4000/$4004/$400C` bit 5, halt, not
   observed until the next length-counter tick). Mesen2 merges both
   deferrals into one slot: `_newHaltValue` is staged alongside
   `_reloadValue` and both are committed at the same `ReloadCounter()`
   call:

   ```
   void ReloadCounter() {
       if(_reloadValue) { … }
       _halt = _newHaltValue;
   }
   ```

   If we adopt approach (b) for test 11, the pending-halt slot for test
   10 is a natural extension of the same field (a `PendingLengthUpdate`
   struct per channel; or two `Option<u8>` fields). They share the
   **commit point**, which is the end of each CPU tick (or, to match
   Mesen2 exactly, just before the frame counter's next step runs in
   the following cycle — our granularity is per-CPU-cycle so the
   difference is nil).

   So yes: the two fixes combine cleanly under one "stage + commit"
   mechanism. Approach (a) does not generalise as nicely — test 10
   needs a similar same-cycle check with different semantics
   ("next-half-frame", which is harder to pre-compute at write time).

7. **Regression risk.**

   The fix only changes the timing of `counter` updates on the four
   length-reload registers. It must not change:
   - writes to $4000/4004/400C bit 5 (halt) other than deferring it
     appropriately under the test-10 fix;
   - the no-op path when `!enabled` (blargg 01 `len_ctr` relies on
     this — `set_enabled(false)` forces counter to 0 at
     `src/apu/length.rs:25-29`);
   - `clock_half_frame` decrement semantics;
   - `is_nonzero`/`length_nonzero` public APIs used by $4015 read.

   Concrete regressions to guard against (must stay green):
   - `apu_test/rom_singles/1-len_ctr.nes` — vanilla reload + decrement
     round-trip. Approach (b) keeps the non-conflict path identical
     (stage on write, commit on the next tick; the commit happens
     *before* any further decrement in the same frame because the
     sequencer's half-frame clocks are spaced >100 cycles apart).
   - `apu_test/rom_singles/2-len_table.nes` — table lookup correctness;
     unaffected (we still index `LENGTH_TABLE`, just at commit time).
   - `blargg_apu_2005.07.30/01.len_ctr.nes` — sanity
     (passes today).
   - `10.len_halt_timing.nes` — will be addressed alongside by sharing
     the commit slot.

   Risk surface is small because (b) is additive: outside the commit
   window, nothing changes.

## 6. Concrete phase-3 fix (approach *b*)

Shape of the change (no code written here — notes only):

1. **`src/apu/length.rs`**
   - Add fields:
     ```rust
     pending_reload: Option<u8>,   // the value written (post-index)
     counter_at_write: u8,         // snapshot of `counter` at write time
     ```
   - Replace `load(load_index, same_cycle_as_clock)` with a
     `stage_reload(load_index)` that stashes the pending value when
     enabled.
   - Add `commit_pending_reload(&mut self)` that runs the Mesen2
     predicate `counter == counter_at_write` before applying.
   - Keep `clock_half_frame` unchanged (sets `length_clocked` field
     becomes unnecessary — delete it from `Apu`).

2. **`src/apu/pulse.rs`, `noise.rs`, `triangle.rs`**
   - `write_timer_hi` / `write_length_load` / `write_length` no longer
     take `length_clocked`; they call `length.stage_reload(value >> 3)`.
   - The non-length side effects on these writes
     (`sequencer_pos = 0`, `envelope.restart()`, `linear_reload_flag =
     true`) remain immediate — those are not part of the glitch.
   - Add `commit_pending_length(&mut self)` that forwards to the length
     counter.

3. **`src/apu/mod.rs`**
   - Remove `length_clocked` field and `write_reg` plumbing.
   - Add an end-of-`tick_cpu_cycle` step (after `clock_half`/`clock_
     quarter` but before the APU/triangle/DMC timer ticks) that calls
     `commit_pending_length` on every channel. Placement note: Mesen2
     commits **after** running the frame counter for the slice but
     **before** running the channels (`NesApu.cpp:162-170`). With our
     CPU-granularity loop, "after frame counter, before channel
     output" falls between the `clock_half`/`clock_quarter` block and
     the `triangle.tick_cpu()` / `pulse.tick_apu()` block at
     `src/apu/mod.rs:131-144`. Insert the commit there.

4. **`src/bus.rs`** — unchanged. The bus still calls `apu.write_reg`
   between pre/post tick. The write just doesn't race against the
   flag anymore because the flag is gone; the glitch rule lives
   entirely inside the APU under approach (b).

5. **Tests to add** (`#[cfg(test)] mod` in `src/apu/length.rs`):
   - Reload **just before** clock → commit applies: counter = loaded
     table value, then decremented by the clock on the subsequent
     half-frame (test 2 path).
   - Reload **just after** clock → commit applies: counter = loaded
     value (test 3 path).
   - Reload **exactly on** clock when counter > 0 → commit drops
     (test 5 path): `counter_at_write = 6`, clock fires → counter = 5
     ≠ `counter_at_write` → drop.
   - Reload **exactly on** clock when counter == 0 → commit applies
     (test 4 path): `counter_at_write = 0`, clock runs with counter
     at 0 which stays 0 → predicate holds → load.

6. **Regression sweep** (per CLAUDE.md mandatory list):
   - `cargo test --lib --release`
   - `instr_test-v5/official_only.nes` — unchanged path, must be 16/16.
   - `instr_misc/instr_misc.nes`
   - All `apu_test/rom_singles/*.nes` — especially `1-len_ctr` and
     `2-len_table`.
   - All `apu_reset/*.nes`.
   - All `cpu_interrupts_v2/rom_singles/*.nes`.
   - `blargg_apu_2005.07.30/11.len_reload_timing.nes` flips to code 1.
   - `blargg_apu_2005.07.30/10.len_halt_timing.nes` probably still
     fails until test-10's `pending_halt` is added — to be done in the
     same commit if cheap; otherwise a clean follow-up.

## 7. Footnote — why NOT approach (a)

At first glance approach (a) ("pre-compute this-cycle's half-frame")
seems simpler: just move the flag-set one step earlier. But:

- `FrameCounter::tick` (src/apu/frame_counter.rs:95-137) has side
  effects on `counter`, `block_ticks_until`, `pending_write`, and
  `mode`. A proper peek needs to duplicate all that logic without
  mutating.
- The `$4017` pending write applies INSIDE `tick` at line 96-117; a
  peek cannot simply return the next step's event without also
  modelling that.
- Even if we had the predicate, it wouldn't help test 10 (pending
  halt) without a matching `next_half_frame` predicate — two
  overlapping predicates instead of one commit slot.
- Mesen2's authors spent effort on this: their comment at
  `NesApu.cpp:160-161` explicitly says they moved to deferred reload
  specifically to fix this test. Inheriting their architectural
  decision is lower-risk than rolling our own predicate.

Therefore: **approach (b) only**. One new per-channel
`PendingLengthUpdate`, one commit point, one predicate. The
`length_clocked` flag goes away.
