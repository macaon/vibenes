# Phase 6 — `blargg_apu_2005.07.30/09.reset_timing.nes` (fail code 4)

Read-only investigation. Target ROM:
`~/Git/nes-test-roms/blargg_apu_2005.07.30/09.reset_timing.nes`.

**Symptom.** Our emulator reports fail code 4 — *"Fourth step occurs
too late"*. Per `tests.txt`: *"After reset or power-up, APU acts as if
`$4017` were written with `$00` from 9 to 12 clocks before first
instruction begins."* Our effective offset is ~4 clocks, which is
**below** the required window; the consequence is that the first
frame-counter step-4 event lands one read-slot later than blargg
tolerates (he probes at instruction-cycle 29818 and 29824 — see below).

---

## 1. Blargg's test — what it actually measures

Source: `~/Git/nes-test-roms/blargg_apu_2005.07.30/source/09.reset_timing.asm`.

Relevant window (lines 22–48 of the .asm):

```
reset:
    ldx   $4015              ; probe A — must read $00 at power-up (code 2)
    ...
    ldy   #25                ; delay 29797 cycles
    lda   #237
    jsr   delay_ya5
    lda   #3                 ; prep "too soon" result
    sta   <result
    lda   $4015              ; read at 29818 — must be $00 (else code 3)
    nop
    ldx   $4015              ; read at 29824 — must be $40 (else code 4)
    ldy   $4015
    cmp   #$00               ; verify A==$00 → "too soon" check
    bne   error
    lda   #4                 ; prep "too late" result
    sta   <result
    cpx   #$40               ; verify X==$40 → "too late" check
    bne   error
```

The comments `read at 29818` and `read at 29824` are the number of CPU
cycles elapsed inside `reset:` at the moment of the `$4015` read cycle.

The pass condition is therefore:

> Step-4 (frame-IRQ set) must latch **strictly between**
> `first_instruction_cycle + 29818` and `first_instruction_cycle + 29824`.

Our read-model caveat: `$4015` clears the frame-IRQ flag, and our APU
ticks at the **end** of the bus access (`Bus::tick_post_access` in
`src/bus.rs:202`). Any "IRQ-set on this cycle" therefore needs to have
happened **before** the read cycle to be visible to the read — the
real 6502 has the same dep­endency, see the comment in
`src/apu/mod.rs:166-176`. For the probe at `first_inst + 29824` to
return `$40`, the step-4 event must happen at or before cycle
`first_inst + 29823`.

---

## 2. Our current effective offset (the actual number)

### 2a. Reset sequence — how many APU ticks before the first opcode fetch

- `Cpu::reset` in `src/cpu/mod.rs:83-105` performs **5 dummy reads**
  of `$00FF` (loop at `src/cpu/mod.rs:92-94`) plus **2 reset-vector
  reads** at `$FFFC/$FFFD` — total **7 bus reads**.
- Every `Bus::read` (`src/bus.rs:100-124`) calls `tick_pre_access`
  then `tick_post_access`; each pair ticks the APU exactly once via
  `self.apu.tick_cpu_cycle()` at `src/bus.rs:203`.
- Net: **7 APU cycles elapse between power-on and the first opcode
  fetch of the first instruction.** After the reset, `apu.cycle == 7`;
  the first instruction's opcode fetch will be the 8th tick (cycle 7 →
  ticks frame counter at input cycle 7, then `apu.cycle` → 8).

### 2b. Frame-counter state through those 7 ticks

From `Apu::new` (`src/apu/mod.rs:60-74`) and `FrameCounter::new`
(`src/apu/frame_counter.rs:46-66`):

```rust
cycle: 0,
pending_write: Some(PendingWrite { value: 0x00, apply_at: 3 }),
counter: 0,
block_ticks_until: 0,
```

Tracing `FrameCounter::tick` (`src/apu/frame_counter.rs:95-137`) with
`self.cycle` as seen by tick (note: `apu.cycle` increments *after*
`tick`, so the `cycle` argument is the pre-increment value):

| tick | arg `cycle` | action | `counter` at end |
|------|-------------|--------|------------------|
| 1 | 0 | not apply; not blocked; bump | 1 |
| 2 | 1 | not apply; not blocked; bump | 2 |
| 3 | 2 | not apply; not blocked; bump | 3 |
| 4 | 3 | **apply $4017=0**: `counter=0`, `block_ticks_until=5`; early-return | 0 |
| 5 | 4 | blocked (`4 < 5`); bump only | 1 |
| 6 | 5 | not blocked; bump; normal step eval | 2 |
| 7 | 6 | not blocked; bump | 3 |

At the end of `Cpu::reset`, `counter == 3`.

### 2c. First instruction, step-4 event, and the probe

- The first opcode fetch is the 8th APU tick (`apu.cycle == 7` when
  tick runs; `counter` becomes 4 at the end of that cycle).
- Step-4 event (frame-IRQ set) is driven by `counter == 29828` — see
  `step_event` table at `src/apu/frame_counter.rs:160`.
- Counter was 0 at the end of cycle 3 (apply tick) and monotonically
  increments per tick. So `counter == 29828` happens at absolute APU
  cycle `3 + 29828 = 29831`.
- Cycles elapsed between first-instruction-fetch (absolute cycle 7)
  and step-4 event (absolute cycle 29831): **29824 CPU cycles**.

### 2d. Why that fails

Blargg's probe at `first_inst + 29824` reads `$4015` on absolute APU
cycle 7 + 29824 = 29831 — **the same cycle** at which our step-4 fires.
Because our `$4015` read observes APU state from the *end of the
previous cycle* (see `Bus::tick_post_access` ordering noted above), the
read returns `$00`. That gives fail code 4.

In other words:
- Effective "$4017 write → first instruction" offset = **7 − 3 = 4 clocks**.
- Blargg requires that offset to be **9 to 12 clocks**.
- We are 5–8 clocks short, and the boundary comparison lands exactly
  1 read-slot on the wrong side.

---

## 3. What the reference emulators do

### 3a. Mesen2 (`~/Git/Mesen2/Core/NES/`)

- `NesCpu::Reset` at `NesCpu.cpp:85-165` — after pushing the reset
  vector into `PC` with a non-ticking `_memoryManager->Read` (see the
  explicit comment at `NesCpu.cpp:99`), it runs **8 full CPU cycles**:

  ```cpp
  // NesCpu.cpp:160-164
  //The CPU takes 8 cycles before it starts executing the ROM's code
  for(int i = 0; i < 8; i++) {
      StartCpuCycle(true);
      EndCpuCycle(true);
  }
  ```

  Each `StartCpuCycle` (`NesCpu.cpp:317-323`) calls
  `_console->ProcessCpuClock()`, which reaches `NesApu::ProcessCpuClock`
  (`NesApu.cpp:219-224`) → `Exec` (`NesApu.cpp:193-201`) → one
  `_currentCycle++` per CPU cycle. So Mesen ticks the APU **8 times**
  before the first opcode fetch.

- `ApuFrameCounter::Reset` at `ApuFrameCounter.h:46-68`:

  ```cpp
  _currentStep = 0;
  _newValue = _stepMode ? 0x80 : 0x00;
  _writeDelayCounter = 3;
  _inhibitIRQ = false;
  _blockFrameCounterTick = 0;
  ```

  The comment at `ApuFrameCounter.h:60-62` says the 9–12-cycle nesdev
  behaviour is *"emulated in the CPU::Reset function"* — i.e., the
  simulated $4017 write is scheduled via `_writeDelayCounter = 3`, and
  the eight reset CPU cycles are what turn that 3 into an effective
  offset.

- Inside `ApuFrameCounter::Run` (`ApuFrameCounter.h:147-164`):

  ```cpp
  if(_newValue >= 0) {
      _writeDelayCounter--;
      if(_writeDelayCounter == 0) { /* apply: resets counters, inhibit */ }
  }
  ```

  `_writeDelayCounter` is decremented **once per outer `Run()` call**,
  and `Run()` is called once per `Exec()`, i.e., once per CPU cycle
  (because `NeedToRun()` at `ApuFrameCounter.h:173-180` returns true
  while `_newValue >= 0`). So:
  - Cycle 1: `_writeDelayCounter` 3→2
  - Cycle 2: 2→1
  - Cycle 3: 1→0 → apply ($4017=0 effect lands)
  - Cycles 4–8: plain sequencer ticks.

  **Net Mesen offset:** apply at cycle 3 of the 8-cycle reset ⇒ 5
  cycles between the simulated-$4017 write and the first opcode
  fetch. (Slightly outside blargg's 9–12 nesdev comment; Mesen does
  pass test 9 on-cart because the probe arithmetic is 1-cycle-tolerant
  in practice — and because Mesen's `$4015` read polls through
  `_apu->Run()` on demand rather than through an end-of-cycle tick,
  so it does observe a same-cycle IRQ set.)

- The same mechanism is used for warm reset (`ApuFrameCounter.h:53-55`
  keeps `_stepMode` unchanged and rewrites it with
  `_newValue = _stepMode ? 0x80 : 0`), and `NesCpu::Reset` still
  executes the 8-cycle loop for soft resets — matching our
  `apu_reset/4017_timing` behaviour.

### 3b. puNES (`~/Git/puNES/src/core/`)

- `cpu_initial_cycles` at `cpu.c:942-955` runs **8 dummy bus reads**
  before the CPU ever fetches a real opcode (3 PC re-reads + 3 stack
  reads + 2 reset-vector reads).
- `apu_turn_on` at `apu.c:213-253` on hard reset zeroes everything
  and then calls:

  ```c
  apu_change_step(apu.step);     // apu.step == 0
  ```

  …which expands (`apu.h:249-250`) to
  `apu.cycles += apuPeriod[apu.mode][apu.type][index]`. With
  `apuPeriod[0][0][0] == 7459` (NTSC, 4-step, index 0 — see
  `apu.h:502-518`), `apu.cycles` starts at **7459**.
- `apu_tick` at `apu.c:38-55` decrements `apu.cycles` at the top of
  every CPU cycle and only runs a step when it hits 0. After the 8
  `cpu_initial_cycles` ticks, `apu.cycles == 7451` (= 7459 − 8).
- puNES's step indices differ from Mesen/ours: the IRQ-set
  (equivalent to our counter==29828 event) is the `case 3` block at
  `apu.c:102-124`, reached after
  7459 + 7456 + 7458 + 7457 = **29830 absolute APU ticks** from power-on.
  That is `29830 − 8 = 29822` cycles after the first opcode fetch.

  So puNES's *"effective $4017 write → first instruction"* offset is
  effectively **8 clocks** (all 8 cycles tick apu.cycles down from
  7459 → 7451 with no "apply delay" to subtract). That lands inside
  blargg's 9–12 window close enough that the probes at +29818 and
  +29824 straddle the step-4 event correctly.

- Warm reset: the `else` branch in `apu_turn_on` at `apu.c:254-278`
  invokes `r4017_jitter(9999)` / `r4017_reset_frame()`. `r4017_jitter`
  (`apu.h:251-275`) sets `r4017.reset_frame_delay = 1` unconditionally
  (plus 1 extra on mode 1), and `r4017_reset_frame` (`apu.h:276-281`)
  decrements it and applies when it hits zero.

### 3c. Nestopia (`~/Git/nestopia/source/core/`)

Nestopia's handling is structurally different from Mesen/puNES — it
doesn't simulate per-cycle APU ticks during reset. Instead:

- `Cpu::Reset` at `NstCpu.cpp:175-237` zeroes registers, sets
  `cycles.count = 0`, and schedules `apu.Reset(hard)` which resets
  cycles.frameCounter to `frameClocks[model][0] * fixed` = **29830 ·
  fixed** (`NstApu.cpp:906-915`, `NstApu.cpp:35-55`).
- `Cpu::Boot` at `NstCpu.cpp:239-255` is the interesting one: it reads
  the reset vector into PC, then calls **`Poke(0x4017, 0x00)`**
  directly (hard) or `Poke(0x4017, apu.GetCtrl())` (soft), and finally
  sets `cycles.count = clock[RESET_CYCLES] + clock[0]` where
  `RESET_CYCLES = 7` (`NstCpu.hpp:105`) → CPU starts execution at
  absolute cycle 8 (= 7 + 1 for the `clock[0]` bump).
- `Apu::WriteFrameCtrl` at `NstApu.cpp:2670-2706` takes the current
  CPU cycle, optionally bumps it for odd parity, adds one clock for
  the write itself, and schedules the frame counter accordingly. So
  Nestopia schedules step-4 at `next + 29830 * clock`, where `next`
  after `Boot()`'s poke is 1 CPU cycle. First instruction runs at
  cycle 8. Delta ≈ 29830 − 7 = **29823 cycles** between the first
  opcode fetch and the step-4 event.

Nestopia's model therefore effectively uses an **~8-cycle** offset
between the simulated $4017 write and the first instruction, same
order of magnitude as puNES.

### 3d. Summary

| Emulator | Reset bus-ticks before first fetch | "$4017 apply" relative to reset start | Effective offset (apply → first fetch) |
|---|---|---|---|
| Mesen2 | 8 | cycle 3 | **5** |
| puNES | 8 | cycle 0 (apu.cycles prefilled) | **8** |
| Nestopia | 7 + 1 = 8 | cycle 0 of CPU execution | **~8** |
| **Ours (today)** | **7** | cycle 3 | **4** |

Blargg's 9–12-clock comment in `tests.txt` is a spec for *power-on
time*, but the passing window in the test ROM itself (the interval
between `+29818` and `+29824` probes) is closer to **≥ 7 and < 13**
cycles of offset. Mesen's 5 barely squeaks through on-cart; puNES's 8
is in the middle; our 4 falls short by at least 1.

---

## 4. Concrete fix — the constant to change

We are **one CPU cycle short of Mesen's 5**, and **4 cycles short of
puNES's 8**. Two independent knobs can fix this; the clean one is:

### Option A (preferred): drop the `apply_at: 3` pending-write pattern

File: `src/apu/frame_counter.rs:46-66`.

Today:
```rust
pending_write: Some(PendingWrite { value: 0x00, apply_at: 3 }),
counter: 0,
```

Change to:
```rust
pending_write: None,
counter: 0,
```

(i.e., power-on is already "apply $4017=0 at cycle 0", with the
counter counting normally from there). Then our effective offset
becomes equal to the number of reset bus-ticks (7), matching the
puNES/Nestopia lower bound.

- `counter` at end of reset = 7 (each of the 7 reset ticks bumps it).
- Step-4 event (counter==29828) lands at absolute APU cycle 29828.
- First instruction at absolute cycle 7.
- Delta = **29821 CPU cycles** — inside blargg's probe window (between
  `+29818` and `+29824`, inclusive of the +29823 boundary we need).

### Option B: widen the reset-cycle count in `Cpu::reset`

File: `src/cpu/mod.rs:92-96`.

Bump the loop from `0..5` to `0..6` (6 dummy reads + 2 vector reads =
**8 total**, matching Mesen/puNES). With `apply_at: 3` kept, that
yields `8 - 3 = 5` cycles of offset — exactly Mesen's model. It would
fix test 9 the same way, but shifts every other test that depends on
the absolute APU cycle count at first instruction by +1, which carries
more regression risk (see §5).

### Option C (not recommended): set `apply_at: -1` or similar

Splitting the `apply_at` into negative territory requires making
`PendingWrite::apply_at` signed or computing it from
`counter.wrapping_sub(n)`; strictly uglier than Option A with no
benefit.

**Recommendation:** Option A. It mirrors puNES (which passes every
blargg APU test we care about) and eliminates a model element
(`pending_write` on fresh power-on) that only existed to patch a
different test.

### Why the comment on `apply_at: 3` is stale

The comment at `src/apu/frame_counter.rs:47-53` justifies the 3-cycle
apply by reference to a `4017_timing` measurement. But:

- The currently-in-tree `apu_reset/4017_timing.nes` is a *warm-reset*
  test — it uses `Apu::reset`'s `reset_on_cpu_reset` path (see §4
  below), which is **independent** of power-on `new()`'s initial
  `pending_write`.
- There is no power-on equivalent in `apu_test/rom_singles/` (no
  `4017_timing.nes` there — see listing). Our `4-jitter.nes` tests
  odd/even parity *during a running frame*, not power-on.

Dropping the power-on `pending_write` should not regress either test.

---

## 4b. Warm reset — `FrameCounter::reset_on_cpu_reset`

File: `src/apu/frame_counter.rs:74-78`.

```rust
pub fn reset_on_cpu_reset(&mut self, cycle: u64) {
    let value = if self.mode == Mode::FiveStep { 0x80 } else { 0 };
    let parity_odd = (cycle & 1) == 1;
    self.write_4017(value, cycle, parity_odd);
}
```

This **stays correct** under Option A. Warm reset is not the same path
as power-on:

- Matches both Mesen2 (`ApuFrameCounter.h:53-67` keeps `_stepMode` and
  sets `_writeDelayCounter = 3`) and puNES (`apu.c:260-263` via
  `r4017_jitter(9999)` which sets `reset_frame_delay = 1`).
- `apu_reset/4017_timing.nes` currently passes and prints "6 cycles"
  (README.md line 102). That's inside the 6–12 range blargg accepts
  for warm reset, independent of Option A.
- `apu_reset/4017_written.nes`, `apu_reset/works_immediately.nes`,
  `apu_reset/irq_flag_cleared.nes`, `apu_reset/4015_cleared.nes`,
  `apu_reset/len_ctrs_enabled.nes` all pass — none of them exercise
  the power-on `new()` path.

So: **no change needed for warm reset.**

---

## 5. Regression risk — what must be re-verified

Option A changes the power-on APU-cycle-count at first-instruction
from effectively "apply at cycle 3; counter==4 at first fetch" to
"apply at cycle 0; counter==7 at first fetch". Every ROM that relies
on the absolute APU cycle at first instruction may shift by 4 cycles
(step-4 event at counter=29828 moves from absolute cycle 29831 →
29828 — 3 cycles earlier; the +2 `block_ticks_until` savings accounts
for the 4th).

### Must-retest (re-run full sweep in `CLAUDE.md §Tests`)

- **`apu_test/rom_singles/*.nes`** — all eight must stay PASS. The
  most sensitive ones are:
  - `3-irq_flag.nes` — samples IRQ over many frames; shift of 3
    cycles inside a 29830-cycle frame is harmless.
  - `4-jitter.nes` — measures even/odd parity *during execution*, not
    power-on; independent.
  - `6-irq_flag_timing.nes` — measures timing of IRQ relative to a
    *runtime* `$4017` write; independent.
- **`apu_reset/*.nes`** — all six:
  - `4017_timing.nes` is a warm-reset test; the printed delay stays
    6 (`reset_on_cpu_reset` path is unchanged). PASS.
  - `4017_written.nes`, `works_immediately.nes`,
    `irq_flag_cleared.nes`, `4015_cleared.nes`,
    `len_ctrs_enabled.nes` — all exercise `Apu::reset`, not `new()`.
- **blargg APU 2005.07.30 01–08** (this same suite):
  - 01 `len_ctr`, 02 `len_table`, 10 `len_halt_timing`,
    11 `len_reload_timing` — length-counter tests; they write
    `$4017` themselves at `setup_apu`/`sync_apu`, so the power-on
    pending-write state is overwritten before measurements start.
    Safe.
  - 03 `irq_flag`, 07 `irq_flag_timing`, 08 `irq_timing` — these call
    `sync_apu` first, so the runtime `$4017` write dominates. Safe.
  - 04 `clock_jitter` — runtime parity measurement after `sync_apu`.
    Safe.
  - 05 `len_timing_mode0`, 06 `len_timing_mode1` — runtime frame
    timing, post-sync. Safe.
- **`instr_test-v5/official_only.nes`** — pure CPU, no APU timing.
  Must stay 16/16.
- **`instr_misc/instr_misc.nes`** — dummy-read timing; no APU-cycle
  dependency.
- **`cpu_interrupts_v2/rom_singles/*.nes`** — phase 5 baseline.
  `1-cli_latency`, `2-nmi_and_brk` should remain unchanged. The
  branch-delays IRQ / IRQ-and-DMA tests exercise the IRQ line timing,
  not the frame-counter phase, so no regression expected.
- **Unit tests** — `cargo test --lib --release`; specifically
  `src/apu/mod.rs` and `src/apu/frame_counter.rs` tests if any rely
  on the `apply_at: 3` seed. Searching the file shows no such test.

### Explicit check-list (per `CLAUDE.md` *Do-before-starting* block)

Re-run **before committing** the Option A fix:

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

Plus the target:
```
./target/release/test_runner \
    ~/Git/nes-test-roms/blargg_apu_2005.07.30/09.reset_timing.nes
```
(Expected: PASS after the fix.)

---

## 6. TL;DR

- **Root cause.** Our power-on `FrameCounter::new` uses
  `pending_write: Some(PendingWrite { value: 0, apply_at: 3 })` paired
  with only 7 reset bus-ticks in `Cpu::reset`. Net: the simulated
  $4017=0 lands only **4 CPU cycles** before the first opcode fetch —
  below blargg's 9–12 window. The step-4 event (counter==29828) thus
  lands one read-slot later than the `$4015` probe at
  `first_inst + 29824` accepts, so the probe reads `$00` → fail code 4.
- **Fix (Option A).** In `src/apu/frame_counter.rs:46-66`, change the
  `pending_write` from `Some(PendingWrite { value: 0x00, apply_at: 3 })`
  to `None`, leaving `counter: 0`. Effective offset becomes 7 cycles,
  delta from first-instruction to step-4 drops from 29824 → 29821 —
  comfortably inside blargg's probe window.
- **Warm reset is untouched.** `FrameCounter::reset_on_cpu_reset`
  (`src/apu/frame_counter.rs:74-78`) already uses the parity-aware
  `write_4017` path, matches Mesen/puNES, and `apu_reset/*.nes`
  passes. Option A changes only the power-on seed state.
- **Regression surface.** All other blargg APU tests either write
  `$4017` at `setup_apu`/`sync_apu` before measuring, or measure
  runtime events; none depend on the power-on pending-write apply
  delay. No regressions expected in `instr_test-v5`, `instr_misc`,
  `cpu_interrupts_v2` (phase 5) — those are CPU-only.

## File/line citations

- `src/apu/frame_counter.rs:46-66` — `FrameCounter::new` (the fix site)
- `src/apu/frame_counter.rs:74-91` — warm-reset + write_4017 parity
- `src/apu/frame_counter.rs:95-137` — `tick` (apply + block semantics)
- `src/apu/frame_counter.rs:149-216` — NTSC/PAL step event table
- `src/apu/mod.rs:60-74` — `Apu::new` initial `cycle: 0`
- `src/apu/mod.rs:89-98` — `Apu::reset` warm-reset path
- `src/apu/mod.rs:166-176` — read-status same-cycle race note
- `src/cpu/mod.rs:83-105` — `Cpu::reset` (5 dummy reads + 2 vector reads)
- `src/bus.rs:100-124` — `Bus::read` ticks pre+post
- `src/bus.rs:175-209` — `tick_pre_access` / `tick_post_access`
- `src/nes.rs:16-26` — `Nes::from_cartridge` (cpu.reset entry point)
- `src/nes.rs:38-44` — `Nes::reset` warm reset path
- `~/Git/Mesen2/Core/NES/APU/ApuFrameCounter.h:46-68` — Mesen reset
- `~/Git/Mesen2/Core/NES/NesCpu.cpp:85-165` — Mesen 8-cycle reset loop
- `~/Git/Mesen2/Core/NES/APU/NesApu.cpp:193-237` — Mesen Exec + Reset
- `~/Git/puNES/src/core/apu.c:38-55, 213-279` — apu_tick, apu_turn_on
- `~/Git/puNES/src/core/apu.h:249-281, 501-541` — apuPeriod + macros
- `~/Git/puNES/src/core/cpu.c:942-955` — cpu_initial_cycles (8 reads)
- `~/Git/nestopia/source/core/NstCpu.cpp:175-255` — Cpu::Reset, Boot
- `~/Git/nestopia/source/core/NstCpu.hpp:105` — `RESET_CYCLES = 7`
- `~/Git/nestopia/source/core/NstApu.cpp:220-290, 906-915, 2670-2706` —
  Apu::Reset, Cycles::Reset, WriteFrameCtrl
- `~/Git/nes-test-roms/blargg_apu_2005.07.30/source/09.reset_timing.asm`
  — the target ROM source
- `~/Git/nes-test-roms/apu_reset/source/4017_timing.s` — warm-reset
  companion test (currently passes, stays passing)
